use crate::api::ApiError;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::OnceCell;

// Bound retained payloads to 64 MiB, with at most 64 entries from one tenant.
const MAX_ENTRIES: usize = 1024;
const MAX_PROJECT_ENTRIES: usize = 64;
const MAX_ENTRY_BYTES: usize = 64 * 1024;
type Key = [u8; 32];
struct Entry {
    project: String,
    created: Instant,
    result: OnceCell<Result<Arc<[u8]>, ApiError>>,
}
pub struct ReportCache {
    ttl: Duration,
    entries: Mutex<HashMap<Key, Arc<Entry>>>,
}
impl ReportCache {
    pub fn new(seconds: u64) -> Self {
        Self {
            ttl: Duration::from_secs(seconds),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub async fn read(
        &self,
        project: &str,
        query: impl Serialize,
        work: impl Future<Output = Result<Value, ApiError>>,
    ) -> Result<Value, ApiError> {
        if self.ttl.is_zero() {
            return work.await;
        }
        let bytes = serde_json::to_vec(&(project, query)).map_err(|_| ApiError::internal())?;
        let key: Key = ring::digest::digest(&ring::digest::SHA256, &bytes)
            .as_ref()
            .try_into()
            .expect("SHA256 length");
        let entry = {
            let mut entries = self.entries.lock().map_err(|_| ApiError::internal())?;
            entries.retain(|_, e| e.created.elapsed() < self.ttl);
            if let Some(entry) = entries.get(&key) {
                Some(entry.clone())
            } else if entries.len() >= MAX_ENTRIES
                || entries.values().filter(|e| e.project == project).count() >= MAX_PROJECT_ENTRIES
            {
                None
            } else {
                let entry = Arc::new(Entry {
                    project: project.into(),
                    created: Instant::now(),
                    result: OnceCell::new(),
                });
                entries.insert(key, entry.clone());
                Some(entry)
            }
        };
        let Some(entry) = entry else {
            return work.await;
        };
        // One initializer performs the query. Other requests wait without taking
        // database permits. Cancellation lets a remaining caller initialize it.
        let result = entry
            .result
            .get_or_init(|| async {
                let value = work.await?;
                serde_json::to_vec(&value)
                    .map(Arc::<[u8]>::from)
                    .map_err(|_| ApiError::internal())
            })
            .await;
        if result
            .as_ref()
            .map_or(true, |bytes| bytes.len() > MAX_ENTRY_BYTES)
        {
            let mut entries = self.entries.lock().map_err(|_| ApiError::internal())?;
            if entries
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
            {
                entries.remove(&key);
            }
        }
        let bytes = result.as_ref().map_err(Clone::clone)?;
        serde_json::from_slice(bytes).map_err(|_| ApiError::internal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn hundred_readers_share_one_query_and_scopes_stay_separate() {
        let cache = ReportCache::new(60);
        let queries = AtomicUsize::new(0);
        let reads = (0..100).map(|_| {
            cache.read("tenant-a", ("summary", "video-a"), async {
                queries.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(json!({"views":7}))
            })
        });
        for result in futures::future::join_all(reads).await {
            assert_eq!(result.unwrap(), json!({"views":7}));
        }
        assert_eq!(queries.load(Ordering::SeqCst), 1);
        let other = cache
            .read("tenant-b", ("summary", "video-a"), async {
                Ok(json!({"views":2}))
            })
            .await
            .unwrap();
        assert_eq!(other["views"], 2);
        let filtered = cache
            .read("tenant-a", ("summary", "video-b"), async {
                Ok(json!({"views":3}))
            })
            .await
            .unwrap();
        assert_eq!(filtered["views"], 3);
    }

    #[tokio::test]
    async fn expiry_failures_and_cancellation_do_not_leave_stale_or_stuck_entries() {
        let cache = Arc::new(ReportCache {
            ttl: Duration::from_millis(20),
            entries: Mutex::new(HashMap::new()),
        });
        cache
            .read("a", "key", async { Ok(json!(1)) })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            cache
                .read("a", "key", async { Ok(json!(2)) })
                .await
                .unwrap(),
            json!(2)
        );
        assert!(
            cache
                .read("a", "error", async { Err(ApiError::unavailable()) })
                .await
                .is_err()
        );
        assert_eq!(
            cache
                .read("a", "error", async { Ok(json!(3)) })
                .await
                .unwrap(),
            json!(3)
        );
        let active_cache = cache.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let active = tokio::spawn(async move {
            active_cache
                .read("a", "cancel", async {
                    let _ = entered.send(());
                    std::future::pending().await
                })
                .await
        });
        ready.await.unwrap();
        active.abort();
        assert!(active.await.unwrap_err().is_cancelled());
        assert_eq!(
            cache
                .read("a", "cancel", async { Ok(json!(4)) })
                .await
                .unwrap(),
            json!(4)
        );
    }

    #[tokio::test]
    async fn one_tenant_and_large_payloads_cannot_fill_the_cache() {
        let cache = ReportCache::new(60);
        for key in 0..100 {
            cache
                .read("a", key, async { Ok(json!(key)) })
                .await
                .unwrap();
        }
        assert_eq!(cache.entries.lock().unwrap().len(), MAX_PROJECT_ENTRIES);
        cache
            .read("b", 1, async { Ok(json!("other tenant")) })
            .await
            .unwrap();
        let huge = json!("x".repeat(MAX_ENTRY_BYTES + 1));
        cache.read("c", 1, async { Ok(huge) }).await.unwrap();
        assert_eq!(
            cache
                .read("c", 1, async { Ok(json!("fresh")) })
                .await
                .unwrap(),
            json!("fresh")
        );
    }
}
