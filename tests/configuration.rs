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

#[test]
fn dotenv_is_automatic_and_process_environment_has_priority() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join(".env"),
        "CHUNKIFY_DATA_DIR=from-dotenv\nCHUNKIFY_BIND=invalid\n",
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
    assert!(fixture.0.join("from-dotenv/keys.json").is_file());
}

#[test]
fn an_explicit_env_file_replaces_dotenv_and_cli_overrides_its_data_directory() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join(".env"), "CHUNKIFY_BIND=invalid\n").unwrap();
    std::fs::write(fixture.0.join("custom.env"), "CHUNKIFY_DATA_DIR=unused\n").unwrap();
    let output = fixture.run(&["--env-file", "custom.env", "init", "--data-dir", "chosen"]);
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("chosen/keys.json").is_file());
    assert!(!fixture.0.join("unused").exists());
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
fn toml_dotenv_environment_and_cli_are_merged_in_priority_order() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("config.toml"),
        "data_dir = 'toml-data'\n[retention]\nseconds = 0\n",
    )
    .unwrap();
    assert!(fixture.run(&["init"]).status.success());
    assert!(fixture.0.join("toml-data/keys.json").is_file());
    std::fs::write(fixture.0.join(".env"), "CHUNKIFY_DATA_DIR=dotenv-data\n").unwrap();
    assert!(fixture.run(&["init"]).status.success());
    assert!(fixture.0.join("dotenv-data/keys.json").is_file());
    let output = fixture
        .command()
        .args(["init"])
        .env("CHUNKIFY_DATA_DIR", "environment-data")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("environment-data/keys.json").is_file());
    let output = fixture
        .command()
        .args(["init", "--data-dir", "cli-data"])
        .env("CHUNKIFY_DATA_DIR", "unused")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("cli-data/keys.json").is_file());
    assert!(!fixture.0.join("unused").exists());
}

#[test]
fn explicit_toml_replaces_default_and_resolves_data_relative_to_its_location() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("config.toml"), "unknown = true").unwrap();
    std::fs::create_dir(fixture.0.join("settings")).unwrap();
    std::fs::write(
        fixture.0.join("settings/production.toml"),
        "data_dir = './state'\n",
    )
    .unwrap();
    let output = fixture.run(&["--config", "settings/production.toml", "init"]);
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("settings/state/keys.json").is_file());
    assert!(!fixture.0.join("state").exists());
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
        "CHUNKIFY_INSERT_MODE",
        "CHUNKIFY_DUCKDB_THREADS",
        "CHUNKIFY_CHECKPOINT_THRESHOLD_MIB",
        "CHUNKIFY_EXPERIMENTAL_READ_WORKER",
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
fn s3_toml_accepts_r2_and_checks_settings_without_connecting_or_printing_secrets() {
    let fixture = Fixture::new();
    let source = "[storage]\nbackend = 's3'\nurl = 's3://analytics/production/'\nendpoint = 'account.r2.cloudflarestorage.com'\nregion = 'auto'\naccess_key_id = 'test-access'\nsecret_access_key = 'private-test-secret'\nurl_style = 'path'\nuse_ssl = true\n";
    std::fs::write(fixture.0.join("config.toml"), source).unwrap();
    let output = fixture.run(&["config", "check"]);
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.0.join("data").exists());
    for invalid in [
        source.replace("backend = 's3'", "backend = 'unknown'"),
        source.replace(
            "url = 's3://analytics/production/'",
            "url = 'https://analytics/production/'",
        ),
        source.replace("secret_access_key = 'private-test-secret'", ""),
        source.replace("url_style = 'path'", "url_style = 'unknown'"),
        source.replace(
            "account.r2.cloudflarestorage.com",
            "https://account.r2.cloudflarestorage.com",
        ),
        format!("{source}\n[experimental]\ndurable_queue = true"),
    ] {
        std::fs::write(fixture.0.join("config.toml"), invalid).unwrap();
        let output = fixture.run(&["config", "check"]);
        assert!(!output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-test-secret"));
    }
    std::fs::write(fixture.0.join("config.toml"), source).unwrap();
    assert!(
        fixture
            .command()
            .args(["config", "check"])
            .env("CHUNKIFY_S3_REGION", "us-east-1")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn storage_backend_dotenv_works_without_toml_and_process_environment_has_priority() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join(".env"), "CHUNKIFY_STORAGE_BACKEND=s3\n").unwrap();
    let output = fixture.run(&["config", "check"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("S3 storage requires storage.url"));

    let output = fixture
        .command()
        .args(["config", "check"])
        .env("CHUNKIFY_S3_URL", "s3://analytics/production/")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!fixture.0.join("data").exists());

    let output = fixture
        .command()
        .args(["config", "check"])
        .env("CHUNKIFY_STORAGE_BACKEND", "local")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn s3_environment_overrides_toml_and_validates_addressing() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("config.toml"),
        "[storage]\nbackend = 'local'\nurl_style = 'invalid'\n",
    )
    .unwrap();
    let overrides = [
        ("CHUNKIFY_STORAGE_BACKEND", "s3"),
        ("CHUNKIFY_S3_URL", "s3://analytics/production/"),
        ("CHUNKIFY_S3_ENDPOINT", "account.r2.cloudflarestorage.com"),
        ("CHUNKIFY_S3_REGION", "auto"),
        ("CHUNKIFY_S3_ACCESS_KEY_ID", "test-access"),
        ("CHUNKIFY_S3_SECRET_ACCESS_KEY", "private-test-secret"),
        ("CHUNKIFY_S3_SESSION_TOKEN", "private-test-token"),
        ("CHUNKIFY_S3_URL_STYLE", "vhost"),
        ("CHUNKIFY_S3_USE_SSL", "true"),
    ];
    let output = fixture
        .command()
        .args(["config", "check"])
        .envs(overrides)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    for (key, value) in [
        ("CHUNKIFY_STORAGE_BACKEND", "invalid"),
        ("CHUNKIFY_STORAGE_BACKEND", ""),
        ("CHUNKIFY_S3_URL_STYLE", "invalid"),
        ("CHUNKIFY_S3_USE_SSL", "invalid"),
        ("CHUNKIFY_S3_URL", "https://invalid"),
        ("CHUNKIFY_S3_ENDPOINT", "https://invalid"),
    ] {
        let output = fixture
            .command()
            .args(["config", "check"])
            .envs(overrides)
            .env(key, value)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("private-test-secret"));
        assert!(!error.contains("private-test-token"));
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
