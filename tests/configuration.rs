use std::{
    path::PathBuf,
    process::{Command, Output},
};

const BINARY: &str = env!("CARGO_BIN_EXE_chunkify-analytics");

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("chunkify-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BINARY);
        command.current_dir(&self.0);
        for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("CHUNKIFY_")) {
            command.env_remove(key);
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn clickhouse() -> String {
    std::env::var("CHUNKIFY_TEST_CLICKHOUSE_URL").unwrap_or_else(|_| "http://127.0.0.1:8123".into())
}

fn sql(query: &str) -> String {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let response = reqwest::Client::new()
            .post(clickhouse())
            .body(query.to_owned())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        response.text().await.unwrap()
    })
}

/// A new database name, dropped with the guard.
struct Database(String);
impl Database {
    fn new() -> Self {
        Self(format!(
            "chunkify_settings_{}",
            uuid::Uuid::new_v4().simple()
        ))
    }
    fn initialized(&self) -> bool {
        sql(&format!(
            "SELECT count() FROM system.tables WHERE database = '{}' AND name = 'registry'",
            self.0
        ))
        .trim()
            == "1"
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        sql(&format!("DROP DATABASE IF EXISTS {}", self.0));
    }
}

#[test]
fn dotenv_is_automatic_and_process_environment_has_priority() {
    let fixture = Fixture::new();
    let database = Database::new();
    std::fs::write(
        fixture.0.join(".env"),
        format!(
            "CHUNKIFY_CLICKHOUSE_URL={}\nCHUNKIFY_CLICKHOUSE_DATABASE={}\nCHUNKIFY_BIND=invalid\n",
            clickhouse(),
            database.0
        ),
    )
    .unwrap();
    assert!(!fixture.run(&["config", "check"]).status.success());
    let output = fixture
        .command()
        .args(["init"])
        .env("CHUNKIFY_BIND", "127.0.0.1:4174")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(database.initialized());
}

#[test]
fn an_explicit_env_file_replaces_dotenv() {
    let fixture = Fixture::new();
    let (chosen, unused) = (Database::new(), Database::new());
    std::fs::write(
        fixture.0.join(".env"),
        format!(
            "CHUNKIFY_BIND=invalid\nCHUNKIFY_CLICKHOUSE_DATABASE={}\n",
            unused.0
        ),
    )
    .unwrap();
    std::fs::write(
        fixture.0.join("custom.env"),
        format!(
            "CHUNKIFY_CLICKHOUSE_URL={}\nCHUNKIFY_CLICKHOUSE_DATABASE={}\n",
            clickhouse(),
            chosen.0
        ),
    )
    .unwrap();
    let output = fixture.run(&["--env-file", "custom.env", "init"]);
    assert!(output.status.success(), "{output:?}");
    assert!(chosen.initialized());
    assert!(!unused.initialized());
}

#[test]
fn missing_explicit_and_malformed_env_files_fail_without_echoing_values() {
    let fixture = Fixture::new();
    let missing = fixture.run(&["--env-file", "missing.env", "config", "check"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Cannot read environment file"));
    std::fs::write(
        fixture.0.join(".env"),
        "CHUNKIFY_MCP_SECRET_KEY='private-test-secret\n",
    )
    .unwrap();
    let output = fixture.run(&["config", "check"]);
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("Invalid environment file"));
    assert!(!message.contains("private-test-secret"));
    assert!(fixture.run(&["--help"]).status.success());
}

#[test]
fn dotenv_does_not_search_parent_directories() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join(".env"), "CHUNKIFY_BIND=invalid\n").unwrap();
    std::fs::create_dir(fixture.0.join("child")).unwrap();
    let output = fixture
        .command()
        .current_dir(fixture.0.join("child"))
        .args(["config", "check"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn toml_dotenv_and_environment_are_merged_in_priority_order() {
    let fixture = Fixture::new();
    let (toml, dotenv, environment) = (Database::new(), Database::new(), Database::new());
    std::fs::write(
        fixture.0.join("config.toml"),
        format!(
            "[clickhouse]\nurl = '{}'\ndatabase = '{}'\n[retention]\nseconds = 0\n",
            clickhouse(),
            toml.0
        ),
    )
    .unwrap();
    assert!(fixture.run(&["init"]).status.success());
    assert!(toml.initialized());
    std::fs::write(
        fixture.0.join(".env"),
        format!("CHUNKIFY_CLICKHOUSE_DATABASE={}\n", dotenv.0),
    )
    .unwrap();
    assert!(fixture.run(&["init"]).status.success());
    assert!(dotenv.initialized());
    let output = fixture
        .command()
        .args(["init"])
        .env("CHUNKIFY_CLICKHOUSE_DATABASE", &environment.0)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(environment.initialized());
}

#[test]
fn explicit_toml_replaces_the_default_file() {
    let fixture = Fixture::new();
    let database = Database::new();
    std::fs::write(fixture.0.join("config.toml"), "unknown = true").unwrap();
    std::fs::create_dir(fixture.0.join("settings")).unwrap();
    std::fs::write(
        fixture.0.join("settings/production.toml"),
        format!(
            "[clickhouse]\nurl = '{}'\ndatabase = '{}'\n",
            clickhouse(),
            database.0
        ),
    )
    .unwrap();
    let output = fixture.run(&["--config", "settings/production.toml", "init"]);
    assert!(output.status.success(), "{output:?}");
    assert!(database.initialized());
}

#[test]
fn unknown_fields_types_and_missing_files_fail_without_echoing_configuration() {
    let fixture = Fixture::new();
    assert!(
        !fixture
            .run(&["--config", "missing.toml", "config", "check"])
            .status
            .success()
    );
    for source in [
        "unknown = 'private-test-secret'",
        "admin_key = 'private-test-secret'",
        "[server]\nbnid = 'private-test-secret'",
        "[retention]\nseconds = 'private-test-secret'",
        "[mcp]\nsecret_key = 'private-test-secret",
    ] {
        std::fs::write(fixture.0.join("config.toml"), source).unwrap();
        let output = fixture.run(&["config", "check"]);
        assert!(!output.status.success());
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(message.contains("Invalid TOML configuration"), "{message}");
        assert!(!message.contains("private-test-secret"));
    }
    assert!(fixture.run(&["--help"]).status.success());
}

#[test]
fn toml_sample_is_complete_and_does_not_create_state_when_checked() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("config.toml"),
        include_str!("../config.toml.example"),
    )
    .unwrap();
    let output = fixture.run(&["config", "check"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.0.join("data").exists());
}

#[test]
fn application_settings_are_toml_only_except_deployment_overrides() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("config.toml"),
        include_str!("../config.toml.example"),
    )
    .unwrap();
    let removed = [
        "CHUNKIFY_DATA_DIR",
        "CHUNKIFY_STORAGE_BACKEND",
        "CHUNKIFY_S3_URL",
        "CHUNKIFY_S3_USE_SSL",
        "CHUNKIFY_LOCAL_PATH",
        "CHUNKIFY_PROFILE",
        "CHUNKIFY_ALLOWED_ORIGINS",
        "CHUNKIFY_STORE_IP",
        "CHUNKIFY_TRUSTED_PROXIES",
        "CHUNKIFY_MAX_BODY_BYTES",
        "CHUNKIFY_MAX_OTLP_RECORDS",
        "CHUNKIFY_INGEST_PER_SECOND",
        "CHUNKIFY_RETENTION_SECONDS",
        "CHUNKIFY_RETENTION_INTERVAL_SECONDS",
        "CHUNKIFY_BATCH_MAX_EVENTS",
        "CHUNKIFY_BATCH_MAX_BYTES",
        "CHUNKIFY_BATCH_DELAY_MS",
        "CHUNKIFY_QUEUE_MAX_BYTES",
        "CHUNKIFY_QUEUE_MAX_REQUESTS",
        "CHUNKIFY_MCP_ENABLED",
        "CHUNKIFY_MCP_ALLOWED_HOSTS",
    ];
    std::fs::write(
        fixture.0.join(".env"),
        removed
            .iter()
            .map(|key| format!("{key}=invalid\n"))
            .collect::<String>(),
    )
    .unwrap();
    let output = fixture
        .command()
        .args(["config", "check"])
        .envs(removed.iter().map(|key| (*key, "invalid")))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.0.join("data").exists());
}

#[test]
fn logging_levels_are_validated_without_echoing_invalid_values() {
    let fixture = Fixture::new();
    for level in ["off", "error", "warn", "info", "debug", "trace"] {
        std::fs::write(
            fixture.0.join("config.toml"),
            format!("[logging]\nlevel = '{level}'\n"),
        )
        .unwrap();
        assert!(fixture.run(&["config", "check"]).status.success());
    }
    for level in ["", "3", "private-test-secret"] {
        std::fs::write(
            fixture.0.join("config.toml"),
            format!("[logging]\nlevel = '{level}'\n"),
        )
        .unwrap();
        let output = fixture.run(&["config", "check"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("logging.level must be"));
        assert!(!error.contains("private-test-secret"));
    }
    assert!(!fixture.0.join("data").exists());
}

#[test]
fn mcp_endpoint_settings_reject_invalid_values_and_empty_host_policies() {
    let fixture = Fixture::new();
    for source in [
        "[mcp]\nenabled = 'yes'",
        "[mcp]\nallowed_hosts = []",
        "[mcp]\nallowed_hosts = ['*']",
        "[mcp]\nallowed_hosts = ['https://example.com/path']",
    ] {
        std::fs::write(fixture.0.join("config.toml"), source).unwrap();
        assert!(!fixture.run(&["config", "check"]).status.success());
    }
    std::fs::write(
        fixture.0.join("config.toml"),
        "[mcp]\nenabled = false\nallowed_hosts = ['analytics.example.com:443']",
    )
    .unwrap();
    assert!(fixture.run(&["config", "check"]).status.success());
}

#[test]
fn clickhouse_environment_overrides_toml_without_printing_secrets() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("config.toml"),
        "[clickhouse]\nurl = 'invalid url'\npassword = 'private-test-secret'\n",
    )
    .unwrap();
    let output = fixture.run(&["config", "check"]);
    assert!(!output.status.success());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("clickhouse.url"), "{message}");
    assert!(!message.contains("private-test-secret"));
    let output = fixture
        .command()
        .args(["config", "check"])
        .env(
            "CHUNKIFY_CLICKHOUSE_URL",
            "https://clickhouse.example.com:8443",
        )
        .env("CHUNKIFY_CLICKHOUSE_DATABASE", "analytics")
        .env("CHUNKIFY_CLICKHOUSE_USER", "chunkify")
        .env("CHUNKIFY_CLICKHOUSE_PASSWORD", "private-test-secret")
        .env("CHUNKIFY_CLICKHOUSE_NODE_ID", "7")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-test-secret"));
    for (name, value) in [
        (
            "CHUNKIFY_CLICKHOUSE_URL",
            "http://user:private-test-secret@clickhouse:8123",
        ),
        ("CHUNKIFY_CLICKHOUSE_DATABASE", "bad-name"),
        ("CHUNKIFY_CLICKHOUSE_NODE_ID", "64"),
    ] {
        let output = fixture
            .command()
            .args(["config", "check"])
            .env("CHUNKIFY_CLICKHOUSE_URL", "http://clickhouse:8123")
            .env(name, value)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{name}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-test-secret"));
    }
}

#[test]
fn public_download_base_rejects_credentials_queries_and_unsupported_schemes() {
    let fixture = Fixture::new();
    for url in [
        "https://analytics.example.com/collector/",
        "http://localhost:4174",
    ] {
        std::fs::write(
            fixture.0.join("config.toml"),
            format!(
                "server.public_url = {}\n",
                serde_json::to_string(url).unwrap()
            ),
        )
        .unwrap();
        assert!(fixture.run(&["config", "check"]).status.success());
    }
    for url in [
        "invalid",
        "file:///tmp/export",
        "https://user:private-test@example.com",
        "https://example.com?key=private-test",
        "https://example.com/#private-test",
    ] {
        std::fs::write(
            fixture.0.join("config.toml"),
            format!(
                "server.public_url = {}\n",
                serde_json::to_string(url).unwrap()
            ),
        )
        .unwrap();
        let output = fixture.run(&["config", "check"]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-test"));
    }
}

#[test]
fn rate_limit_overrides_validate_without_starting_storage() {
    let fixture = Fixture::new();
    for source in [
        "[rate_limits]\nread_per_minute=1\nread_burst=1\n",
        "[rate_limits]\nread_concurrent_per_project=3\nread_concurrent=4\nexport_concurrent_per_project=2\nexport_concurrent=3\nexport_links_per_minute=60\nexport_downloads_per_minute=30\n",
    ] {
        std::fs::write(fixture.0.join("config.toml"), source).unwrap();
        let output = fixture.run(&["config", "check"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for (field, value) in [
        ("read_per_minute", "0"),
        ("read_burst", "0"),
        ("read_concurrent_per_project", "0"),
        ("read_concurrent", "0"),
        ("export_links_per_minute", "0"),
        ("export_downloads_per_minute", "0"),
        ("export_concurrent_per_project", "0"),
        ("export_concurrent", "0"),
        ("read_per_minute", "1000001"),
        ("read_burst", "1000001"),
        ("read_concurrent", "257"),
        ("export_concurrent", "65"),
        ("read_concurrent_per_project", "33"),
        ("export_concurrent_per_project", "5"),
        ("read_per_minute", "-1"),
        ("read_burst", "1.5"),
        ("unknown", "1"),
    ] {
        std::fs::write(
            fixture.0.join("config.toml"),
            format!("[rate_limits]\n{field}={value}\n"),
        )
        .unwrap();
        let output = fixture.run(&["config", "check"]);
        assert!(!output.status.success(), "accepted {field}={value}");
    }
    assert!(!fixture.0.join("data").exists());
}

#[test]
fn report_cache_freshness_is_bounded_to_one_minute() {
    let fixture = Fixture::new();
    for (value, valid) in [
        ("0", true),
        ("1", true),
        ("60", true),
        ("61", false),
        ("-1", false),
        ("1.5", false),
    ] {
        std::fs::write(
            fixture.0.join("config.toml"),
            format!("cache.report_ttl_seconds={value}\n"),
        )
        .unwrap();
        let output = fixture.run(&["config", "check"]);
        assert_eq!(output.status.success(), valid, "cache TTL {value}");
    }
    assert!(!fixture.0.join("data").exists());
}

#[test]
fn geolocation_path_is_relative_to_toml_and_environment_can_override_it() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.0.join("settings")).unwrap();
    std::fs::write(
        fixture.0.join("settings/server.toml"),
        "[server]\ngeoip_database = 'missing-city.mmdb'\n",
    )
    .unwrap();
    let output = fixture.run(&["--config", "settings/server.toml", "serve"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("settings/missing-city.mmdb"));
    let output = fixture
        .command()
        .args(["--config", "settings/server.toml", "serve"])
        .env("CHUNKIFY_GEOIP_DATABASE", "override-city.mmdb")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("override-city.mmdb"));
    assert!(!stderr.contains("settings/missing-city.mmdb"));
    assert!(!fixture.0.join("data").exists());
}
