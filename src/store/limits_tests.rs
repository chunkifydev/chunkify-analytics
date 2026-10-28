use super::*;
use std::{sync::mpsc, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_clients_keep_database_concurrency_permits_until_work_finishes() {
    for separate_reads in [false, true] {
        let directory =
            std::env::temp_dir().join(format!("chunkify-read-limits-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let env = directory.join("empty.env");
        let file = directory.join("config.toml");
        std::fs::write(&env, "").unwrap();
        std::fs::write(&file, format!("retention.seconds=0\nexperimental.read_worker={separate_reads}\nrate_limits.read_concurrent_per_project=1\nrate_limits.read_concurrent=1\n")).unwrap();
        let settings = crate::settings::Settings::load(Some(&env), Some(&file)).unwrap();
        let config = crate::config::Config::load_with(&settings, Some(&directory), None).unwrap();
        let keys = crate::config::Keys {
            public_key: "test-public".into(),
            secret_key: "test-secret".into(),
            projects: None,
        };
        let projects = Arc::new(crate::projects::Registry::new(&keys, &config).unwrap());
        let (store, worker) = Store::from_config(&config, projects).unwrap();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = mpsc::channel();
        let task_store = store.clone();
        let client = tokio::spawn(async move {
            task_store
                .read("blocked", "default".into(), move |_| {
                    let _ = entered.send(());
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        client.abort();
        assert!(client.await.unwrap_err().is_cancelled());
        let rejected = store
            .events("default".into(), Query::default())
            .await
            .unwrap_err();
        assert_eq!(rejected.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(rejected.retry_after, Some(1));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if store
                    .events("default".into(), Query::default())
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(store);
        worker.join().unwrap().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
