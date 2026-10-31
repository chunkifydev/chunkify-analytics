//! Opt-in S3 buffering. Reads never flush the buffer or reset its deadline.
use super::*;
use std::collections::VecDeque;

pub(super) fn run(
    connection: &mut Connection,
    receiver: mpsc::Receiver<Job>,
    config: &BatchConfig,
    remote: &crate::s3_store::RemoteStore,
    profile: &mut Profile,
) -> Result<(), String> {
    let interval = remote.flush_interval();
    let mut waiting = VecDeque::<WriteRequest>::new();
    let mut batch = Vec::new();
    let mut records = 0;
    let mut bytes = 0;
    let mut deadline = Instant::now();
    let mut state = RemoteBatch::default();
    let mut retrying = false;
    let mut closing = false;
    loop {
        // A failed publication keeps its exact stage and object names. Later
        // requests keep their admission permits while waiting behind that batch.
        if !retrying {
            while let Some(next) = waiting.front() {
                if records + next.rows.len() > config.max_events
                    || bytes + next.bytes > config.max_bytes
                {
                    break;
                }
                let request = waiting.pop_front().expect("waiting request");
                if batch.is_empty() {
                    deadline = request.queued + interval;
                }
                records += request.rows.len();
                bytes += request.bytes;
                batch.push(request);
            }
        }
        let full = records >= config.max_events || bytes >= config.max_bytes || !waiting.is_empty();
        if !batch.is_empty() && (closing || Instant::now() >= deadline || (!retrying && full)) {
            match save_batch(
                connection,
                &batch,
                (records, bytes),
                config.bulk,
                Some(remote),
                &mut state,
                profile,
            ) {
                Ok(_) => {
                    batch.clear();
                    records = 0;
                    bytes = 0;
                    state = RemoteBatch::default();
                    retrying = false;
                }
                Err(_) if closing => {
                    return Err("Cannot upload buffered S3 events during shutdown".into());
                }
                Err(_) => {
                    retrying = true;
                    deadline = Instant::now() + interval;
                }
            }
            continue;
        }
        if closing {
            return Ok(());
        }
        let job = if batch.is_empty() {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        };
        match job {
            Ok(Job::Write(request)) => waiting.push_back(request),
            Ok(Job::Call(operation)) => operation(connection, profile),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => closing = true,
        }
    }
}
