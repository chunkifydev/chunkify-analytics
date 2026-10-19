use crate::{api::ApiError, profile::Profile};
use duckdb::Connection;
use std::{sync::mpsc, thread, time::Instant};
use tokio::sync::oneshot;

// One executing read and at most 64 waiting reads. Saturation returns HTTP 503.
const QUEUE_CAPACITY: usize = 64;
type Operation = Box<dyn FnOnce(&mut Connection, &mut Profile) + Send>;

#[derive(Clone)]
pub struct Reader {
    sender: mpsc::SyncSender<Operation>,
    profile: bool,
}

impl Reader {
    pub fn start(
        mut connection: Connection,
        profile: bool,
    ) -> duckdb::Result<(Self, thread::JoinHandle<()>)> {
        let (sender, receiver) = mpsc::sync_channel::<Operation>(QUEUE_CAPACITY);
        let mut measurements = Profile::new(profile, &connection, "reader")?;
        let worker = thread::spawn(move || {
            while let Ok(operation) = receiver.recv() {
                operation(&mut connection, &mut measurements);
            }
            measurements.report();
        });
        Ok((Self { sender, profile }, worker))
    }

    pub async fn call<T: Send + 'static>(
        &self,
        name: &'static str,
        operation: impl FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let (sender, receiver) = oneshot::channel();
        let queued = self.profile.then(Instant::now);
        self.sender
            .try_send(Box::new(move |connection, profile| {
                // Do not execute reads whose HTTP caller has already gone away.
                if sender.is_closed() {
                    return;
                }
                let started = profile.start();
                let result = snapshot(connection, operation);
                profile.job(name, queued, started);
                let _ = sender.send(result);
            }))
            .map_err(|_| ApiError::unavailable())?;
        receiver.await.map_err(|_| ApiError::unavailable())?
    }
}

// Keep the page watermark and result query in the same snapshot while ingestion
// and retention run on another connection. The transaction ends before replying.
pub fn snapshot<T>(
    connection: &mut Connection,
    operation: impl FnOnce(&Connection) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let tx = connection.transaction()?;
    let result = operation(&tx)?;
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{Future, poll_fn},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    #[tokio::test]
    async fn reads_keep_a_snapshot_without_blocking_commits_and_recover_after_errors() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE items (id INTEGER); INSERT INTO items VALUES (1)")
            .unwrap();
        let (reader, worker) = Reader::start(connection.try_clone().unwrap(), false).unwrap();
        let (entered, ready) = oneshot::channel();
        let (release, blocked) = mpsc::channel();
        let active_reader = reader.clone();
        let active = tokio::spawn(async move {
            active_reader
                .call("test_snapshot", move |db| {
                    let before: i64 =
                        db.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
                    let _ = entered.send(());
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    let after: i64 =
                        db.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
                    Ok((before, after))
                })
                .await
        });
        ready.await.unwrap();
        let (committed, done) = oneshot::channel();
        let writer = thread::spawn(move || {
            // Exercise retention as well as an append while a read is active.
            let result = connection.execute_batch(
                "BEGIN; DELETE FROM items; INSERT INTO items VALUES (2), (3); COMMIT",
            );
            let _ = committed.send(result.is_ok());
        });
        let result = tokio::time::timeout(Duration::from_secs(1), done).await;
        release.send(()).unwrap();
        let values = active.await.unwrap().ok();
        writer.join().unwrap();
        assert!(
            result.is_ok_and(|result| result == Ok(true)),
            "writer must commit while a read snapshot is open"
        );
        assert_eq!(
            values,
            Some((1, 1)),
            "both reads must use the pre-commit snapshot"
        );
        assert!(
            reader
                .call::<()>("test_error", |db| {
                    db.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))?;
                    Err(ApiError::bad("test failure"))
                })
                .await
                .is_err()
        );
        assert_eq!(
            reader
                .call("test_next_snapshot", |db| Ok(db.query_row(
                    "SELECT COUNT(*) FROM items",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .await
                .ok(),
            Some(2)
        );
        drop(reader);
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn read_queue_is_bounded_and_canceled_reads_are_not_executed() {
        let (reader, worker) = Reader::start(Connection::open_in_memory().unwrap(), false).unwrap();
        let (entered, ready) = oneshot::channel();
        let (release, blocked) = mpsc::channel();
        let active_reader = reader.clone();
        let active = tokio::spawn(async move {
            active_reader
                .call("test_blocked_read", move |_| {
                    let _ = entered.send(());
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        let executed = Arc::new(AtomicUsize::new(0));
        let mut waiting = Vec::new();
        for _ in 0..QUEUE_CAPACITY {
            let executed = executed.clone();
            let mut call = Box::pin(reader.call("test_canceled_read", move |_| {
                executed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }));
            // Poll once to enqueue, retaining the future so it can be canceled.
            assert!(poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx).is_pending())).await);
            waiting.push(call);
        }
        let overflow = reader.call("test_overflow", |_| Ok(())).await;
        assert!(
            overflow.is_err_and(|error| error.0 == axum::http::StatusCode::SERVICE_UNAVAILABLE)
        );
        drop(waiting);
        release.send(()).unwrap();
        assert!(active.await.unwrap().is_ok());
        drop(reader);
        worker.join().unwrap();
        assert_eq!(executed.load(Ordering::SeqCst), 0);
    }
}
