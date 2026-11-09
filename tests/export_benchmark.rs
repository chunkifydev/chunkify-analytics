//! Manual CSV throughput check against a disposable ClickHouse database
//! (CHUNKIFY_TEST_CLICKHOUSE_URL, default http://127.0.0.1:8123).
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
const BINARY: &str = env!("CARGO_BIN_EXE_chunkify-analytics");

struct Fixture {
    directory: PathBuf,
    child: Option<Child>,
}
impl Fixture {
    fn command(&self) -> Command {
        let mut command = Command::new(BINARY);
        command.current_dir(&self.directory);
        for (key, _) in std::env::vars().filter(|(k, _)| k.starts_with("CHUNKIFY_")) {
            command.env_remove(key);
        }
        command
    }
    fn start(&mut self) -> String {
        let mut child = self
            .command()
            .arg("serve")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env(
                "CHUNKIFY_GEOIP_DATABASE",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/GeoIP2-City-Test.mmdb"
                ),
            )
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let url = loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if let Some(url) = line.trim().strip_prefix("Listening on ") {
                break url.to_owned();
            }
        };
        self.child = Some(child);
        url
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            assert!(child.wait().unwrap().success());
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
// The ClickHouse database is left for inspection; drop it after the run.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual CSV throughput benchmark; run with --ignored --nocapture"]
async fn stream_large_exports_while_ingestion_continues() {
    let count = std::env::var("CHUNKIFY_EXPORT_BENCHMARK_ROWS")
        .ok()
        .map(|v| v.parse::<usize>().unwrap())
        .unwrap_or(1_000_000);
    let directory = std::env::temp_dir().join(format!(
        "chunkify-export-benchmark-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&directory).unwrap();
    let clickhouse = std::env::var("CHUNKIFY_TEST_CLICKHOUSE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8123".into());
    let database = format!(
        "chunkify_export_benchmark_{}",
        uuid::Uuid::new_v4().simple()
    );
    std::fs::write(directory.join("config.toml"), format!("server.bind=\"127.0.0.1:0\"\nserver.ingest_per_second=10000\nretention.seconds=0\nbatch.delay_ms=1\nmcp.enabled=false\nrate_limits.export_downloads_per_minute=1000\nclickhouse.url={clickhouse:?}\nclickhouse.database={database:?}\n")).unwrap();
    let mut fixture = Fixture {
        directory,
        child: None,
    };
    let output = fixture.command().arg("init").output().unwrap();
    assert!(output.status.success());
    let keys: Value = serde_json::from_slice(&output.stdout).unwrap();
    let started = Instant::now();
    {
        // Seed directly with one INSERT ... SELECT. Seeded event orders are
        // small, so live events sort after them.
        let payload =
            json!({"benchmark_payload":"x".repeat(256),"sdk_name":"benchmark","sdk_version":"1"})
                .to_string();
        let from = chrono::DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        let now = chrono::Utc::now().timestamp_millis();
        let mut target = reqwest::Url::parse(&clickhouse).unwrap();
        target
            .query_pairs_mut()
            .append_pair("database", &database)
            .append_pair("param_payload", &payload);
        let response = reqwest::Client::new()
            .post(target)
            .body(format!("INSERT INTO events (ingest_id,event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,context_browser_name,playing_time_ms)
                SELECT number + 1, concat('event-', toString(number)), 'default', concat('view-', toString(number)), concat('video-', toString(number % 1000)), 'benchmark', 'chunkify.video.view_started', 1, {from} + (number % 2592000) * 1000, {now}, {{payload:String}}, 0, if(number % 2 = 0, 'Chrome', 'Safari'), 0 FROM numbers({count})"))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
    }
    eprintln!(
        "Seeded {count} synthetic events across a 30-day timestamp range in {:.2}s",
        started.elapsed().as_secs_f64()
    );
    let url = fixture.start();
    let pid = fixture.child.as_ref().unwrap().id();
    let active = Arc::new(AtomicBool::new(true));
    let peak = Arc::new(AtomicU64::new(0));
    let monitoring = active.clone();
    let maximum = peak.clone();
    let monitor = std::thread::spawn(move || {
        while monitoring.load(Ordering::Relaxed) {
            if let Ok(output) = Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                && let Ok(kib) = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u64>()
            {
                maximum.fetch_max(kib, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let client = reqwest::Client::new();
    let token = keys["secret_key"].as_str().unwrap();
    let contract: Value = serde_json::from_str(include_str!("../openapi.json")).unwrap();
    let mut probe=contract["paths"]["/v1/events"]["post"]["requestBody"]["content"]["application/json"]["example"].clone();
    probe["video_id"] = json!("live-probe");
    for resource in ["events", "views", "videos"] {
        let descriptor = client
            .post(format!("{url}/v1/exports"))
            .bearer_auth(token)
            .json(&json!({"resource":resource,"query":{"viewer_id":"benchmark"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(descriptor.status(), 201);
        let link: Value = descriptor.json().await.unwrap();
        let began = Instant::now();
        let mut response = client
            .get(link["download_url"].as_str().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut bytes = 0usize;
        let mut rows = 0usize;
        let mut first_data = None;
        let mut writes = Vec::new();
        while let Some(chunk) = response.chunk().await.unwrap() {
            bytes += chunk.len();
            rows += chunk.iter().filter(|b| **b == b'\n').count();
            if rows > 1 && first_data.is_none() {
                first_data = Some(began.elapsed());
                if resource == "events" {
                    for _ in 0..10 {
                        probe["event_id"] = json!(uuid::Uuid::new_v4().to_string());
                        probe["view_id"] = json!(uuid::Uuid::new_v4().to_string());
                        let sent = Instant::now();
                        let result = client
                            .post(format!("{url}/v1/events"))
                            .header(
                                "X-Chunkify-Ingest-Key",
                                keys["public_key"].as_str().unwrap(),
                            )
                            .json(&probe)
                            .send()
                            .await
                            .unwrap();
                        assert_eq!(result.status(), 201);
                        writes.push(sent.elapsed().as_secs_f64() * 1000.0);
                    }
                }
            }
        }
        let expected = if resource == "videos" {
            count.min(1000)
        } else {
            count
        };
        assert_eq!(rows, expected + 1);
        writes.sort_by(f64::total_cmp);
        eprintln!(
            "{resource}: {expected} rows, {:.1} MiB, first data {:.3}s, total {:.3}s, peak server RSS {:.1} MiB, ingestion p95 {:?} ms",
            bytes as f64 / 1048576.0,
            first_data.unwrap().as_secs_f64(),
            began.elapsed().as_secs_f64(),
            peak.load(Ordering::Relaxed) as f64 / 1024.0,
            writes.get(9)
        );
    }
    active.store(false, Ordering::Relaxed);
    monitor.join().unwrap();
    fixture.stop();
}
