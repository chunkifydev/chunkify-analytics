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
        for (key, _) in std::env::vars()
            .filter(|(key, _)| key.starts_with("COLLECTOR_") || key.starts_with("CHUNKIFY_MCP_"))
        {
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
        "COLLECTOR_DATA_DIR=from-dotenv\nCOLLECTOR_BIND=invalid\n",
    )
    .unwrap();
    assert!(!fixture.run(&["config", "check"]).status.success());
    let output = fixture
        .command()
        .args(["init"])
        .env("COLLECTOR_BIND", "127.0.0.1:4174")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("from-dotenv/keys.json").is_file());
}

#[test]
fn an_explicit_env_file_replaces_dotenv_and_cli_overrides_its_data_directory() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join(".env"), "COLLECTOR_BIND=invalid\n").unwrap();
    std::fs::write(fixture.0.join("custom.env"), "COLLECTOR_DATA_DIR=unused\n").unwrap();
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
    std::fs::write(fixture.0.join(".env"), "COLLECTOR_BIND=invalid\n").unwrap();
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
        fixture.0.join("collector.toml"),
        "data_dir = 'toml-data'\n[retention]\nseconds = 0\n",
    )
    .unwrap();
    assert!(fixture.run(&["init"]).status.success());
    assert!(fixture.0.join("toml-data/keys.json").is_file());
    std::fs::write(fixture.0.join(".env"), "COLLECTOR_DATA_DIR=dotenv-data\n").unwrap();
    assert!(fixture.run(&["init"]).status.success());
    assert!(fixture.0.join("dotenv-data/keys.json").is_file());
    let output = fixture
        .command()
        .args(["init"])
        .env("COLLECTOR_DATA_DIR", "environment-data")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("environment-data/keys.json").is_file());
    let output = fixture
        .command()
        .args(["init", "--data-dir", "cli-data"])
        .env("COLLECTOR_DATA_DIR", "unused")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("cli-data/keys.json").is_file());
    assert!(!fixture.0.join("unused").exists());
}

#[test]
fn explicit_toml_replaces_default_and_resolves_data_relative_to_its_location() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("collector.toml"), "unknown = true").unwrap();
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
        std::fs::write(fixture.0.join("collector.toml"), source).unwrap();
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
        fixture.0.join("collector.toml"),
        include_str!("../collector.toml.example"),
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
        fixture.0.join("collector.toml"),
        include_str!("../collector.toml.example"),
    )
    .unwrap();
    let removed = [
        "COLLECTOR_STORAGE_BACKEND",
        "COLLECTOR_LOCAL_PATH",
        "COLLECTOR_PROFILE",
        "COLLECTOR_ALLOWED_ORIGINS",
        "COLLECTOR_STORE_IP",
        "COLLECTOR_TRUSTED_PROXIES",
        "COLLECTOR_MAX_BODY_BYTES",
        "COLLECTOR_MAX_OTLP_RECORDS",
        "COLLECTOR_INGEST_PER_SECOND",
        "COLLECTOR_RETENTION_SECONDS",
        "COLLECTOR_RETENTION_INTERVAL_SECONDS",
        "COLLECTOR_BATCH_MAX_EVENTS",
        "COLLECTOR_BATCH_MAX_BYTES",
        "COLLECTOR_BATCH_DELAY_MS",
        "COLLECTOR_QUEUE_MAX_BYTES",
        "COLLECTOR_QUEUE_MAX_REQUESTS",
        "COLLECTOR_INSERT_MODE",
        "COLLECTOR_DUCKDB_THREADS",
        "COLLECTOR_CHECKPOINT_THRESHOLD_MIB",
        "COLLECTOR_EXPERIMENTAL_READ_WORKER",
        "COLLECTOR_MCP_ENABLED",
        "COLLECTOR_MCP_ALLOWED_HOSTS",
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
fn mcp_endpoint_settings_reject_invalid_values_and_empty_host_policies() {
    let fixture = Fixture::new();
    for source in [
        "[mcp]\nenabled = 'yes'",
        "[mcp]\nallowed_hosts = []",
        "[mcp]\nallowed_hosts = ['*']",
        "[mcp]\nallowed_hosts = ['https://example.com/path']",
    ] {
        std::fs::write(fixture.0.join("collector.toml"), source).unwrap();
        assert!(!fixture.run(&["config", "check"]).status.success());
    }
    std::fs::write(
        fixture.0.join("collector.toml"),
        "[mcp]\nenabled = false\nallowed_hosts = ['analytics.example.com:443']",
    )
    .unwrap();
    assert!(fixture.run(&["config", "check"]).status.success());
}

#[test]
fn s3_toml_accepts_r2_and_checks_settings_without_connecting_or_printing_secrets() {
    let fixture = Fixture::new();
    let source = "[storage]\nbackend = 's3'\nurl = 's3://analytics/production/'\nendpoint = 'account.r2.cloudflarestorage.com'\nregion = 'auto'\naccess_key_id = 'test-access'\nsecret_access_key = 'private-test-secret'\nurl_style = 'path'\nuse_ssl = true\n";
    std::fs::write(fixture.0.join("collector.toml"), source).unwrap();
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
        format!("{source}\n[experimental]\nread_worker = true"),
    ] {
        std::fs::write(fixture.0.join("collector.toml"), invalid).unwrap();
        let output = fixture.run(&["config", "check"]);
        assert!(!output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-test-secret"));
    }
    std::fs::write(fixture.0.join("collector.toml"), source).unwrap();
    assert!(
        fixture
            .command()
            .args(["config", "check"])
            .env("COLLECTOR_S3_REGION", "us-east-1")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn s3_environment_overrides_toml_and_validates_addressing() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join("collector.toml"),
        "[storage]\nbackend = 's3'\nurl_style = 'invalid'\n",
    )
    .unwrap();
    let overrides = [
        ("COLLECTOR_S3_URL", "s3://analytics/production/"),
        ("COLLECTOR_S3_ENDPOINT", "account.r2.cloudflarestorage.com"),
        ("COLLECTOR_S3_REGION", "auto"),
        ("COLLECTOR_S3_ACCESS_KEY_ID", "test-access"),
        ("COLLECTOR_S3_SECRET_ACCESS_KEY", "private-test-secret"),
        ("COLLECTOR_S3_SESSION_TOKEN", "private-test-token"),
        ("COLLECTOR_S3_URL_STYLE", "vhost"),
        ("COLLECTOR_S3_USE_SSL", "true"),
    ];
    let output = fixture
        .command()
        .args(["config", "check"])
        .envs(overrides)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    for (key, value) in [
        ("COLLECTOR_S3_URL_STYLE", "invalid"),
        ("COLLECTOR_S3_USE_SSL", "invalid"),
        ("COLLECTOR_S3_URL", "https://invalid"),
        ("COLLECTOR_S3_ENDPOINT", "https://invalid"),
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
