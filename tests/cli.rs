//! Command-line behavior. Commands that store projects use a new database on
//! the ClickHouse server at CHUNKIFY_TEST_CLICKHOUSE_URL (default local).
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_chunkify-analytics");

fn clickhouse() -> String {
    std::env::var("CHUNKIFY_TEST_CLICKHOUSE_URL").unwrap_or_else(|_| "http://127.0.0.1:8123".into())
}

fn database() -> String {
    format!("chunkify_cli_{}", uuid::Uuid::new_v4().simple())
}

fn run_in(database: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(BINARY);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("CHUNKIFY_")) {
        command.env_remove(key);
    }
    if let Some(database) = database {
        command
            .env("CHUNKIFY_CLICKHOUSE_URL", clickhouse())
            .env("CHUNKIFY_CLICKHOUSE_DATABASE", database);
    }
    let directory = std::env::temp_dir().join(format!("chunkify-cli-cwd-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let output = command.current_dir(&directory).args(args).output().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    output
}

fn run(args: &[&str]) -> Output {
    run_in(None, args)
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

/// Every stored registry version, oldest first.
fn registry(database: &str) -> Vec<String> {
    sql(&format!(
        "SELECT document FROM {database}.registry ORDER BY version FORMAT JSONEachRow"
    ))
    .lines()
    .map(|line| {
        serde_json::from_str::<serde_json::Value>(line).unwrap()["document"]
            .as_str()
            .unwrap()
            .to_owned()
    })
    .collect()
}

struct Database(String);
impl Drop for Database {
    fn drop(&mut self) {
        sql(&format!("DROP DATABASE IF EXISTS {}", self.0));
    }
}

#[test]
fn help_and_version_work_without_valid_server_configuration() {
    for args in [
        vec!["--help"],
        vec!["project", "--help"],
        vec!["key", "rotate", "--help"],
        vec!["mcp", "--help"],
        vec!["--version"],
    ] {
        let output = Command::new(BINARY)
            .args(&args)
            .env("CHUNKIFY_BIND", "invalid")
            .env("CHUNKIFY_CLICKHOUSE_URL", "invalid")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("chunkify-analytics"));
    }
}

#[test]
fn missing_arguments_and_invalid_permissions_fail_in_the_parser() {
    for args in [
        vec![],
        vec!["project", "create"],
        vec!["key", "create", "default", "admin", "wrong"],
        vec!["viewer", "delete"],
        vec!["serve", "--bind", "invalid"],
        vec!["project-list"],
        vec!["init", "--data-dir", "/removed-option"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

#[test]
fn flags_override_environment_and_configuration_check_contacts_no_database() {
    let output = Command::new(BINARY)
        .args(["serve", "--bind", "127.0.0.1:0", "--help"])
        .env("CHUNKIFY_BIND", "invalid")
        .output()
        .unwrap();
    assert!(output.status.success());
    // An unreachable server is fine: checking configuration does not connect.
    let output = Command::new(BINARY)
        .args(["config", "check"])
        .env("CHUNKIFY_CLICKHOUSE_URL", "http://127.0.0.1:1")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Configuration is valid."
    );
}

#[test]
fn clickhouse_settings_are_validated_without_contacting_the_server() {
    let directory = std::env::temp_dir().join(format!(
        "chunkify-clickhouse-config-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&directory).unwrap();
    let config = directory.join("config.toml");
    for (setting, valid) in [
        ("url = \"https://clickhouse.example.com:8443/\"", true),
        ("url = \"http://user:secret@clickhouse:8123\"", false),
        ("url = \"ftp://clickhouse:8123\"", false),
        ("database = \"analytics_2\"", true),
        ("database = \"analytics; DROP\"", false),
        ("database = \"2analytics\"", false),
        ("node_id = 63", true),
        ("node_id = 64", false),
        ("registry_refresh_seconds = 0", false),
        ("deduplication_window_seconds = 0", true),
        ("unknown = true", false),
    ] {
        std::fs::write(&config, format!("[clickhouse]\n{setting}\n")).unwrap();
        let output = run(&["--config", config.to_str().unwrap(), "config", "check"]);
        assert_eq!(output.status.success(), valid, "{setting}: {output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("secret"), "{error}");
    }
    for removed in [
        "data_dir = \"data\"",
        "[storage]\nbackend = \"local\"",
        "[duckdb]\nthreads = 1",
        "[experimental]\nread_worker = true",
        "profile = true",
    ] {
        std::fs::write(&config, format!("{removed}\n")).unwrap();
        let output = run(&["--config", config.to_str().unwrap(), "config", "check"]);
        assert!(!output.status.success(), "{removed}");
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn initialization_and_primary_rotation_return_secrets_without_persisting_them() {
    let database = Database(database());
    let db = Some(database.0.as_str());
    let initialized = run_in(db, &["init"]);
    assert!(initialized.status.success(), "{initialized:?}");
    let initial: serde_json::Value = serde_json::from_slice(&initialized.stdout).unwrap();
    let public = initial["public_key"].as_str().unwrap();
    let secret = initial["secret_key"].as_str().unwrap();
    for kind in ["public", "secret"] {
        let rotated = run_in(db, &["key", "rotate-primary", kind]);
        assert!(rotated.status.success(), "{rotated:?}");
        let value: serde_json::Value = serde_json::from_slice(&rotated.stdout).unwrap();
        let raw = value["key"].as_str().unwrap();
        assert_ne!(raw, public);
        assert_ne!(raw, secret);
        assert_eq!(value["key_id"], format!("primary-{kind}"));
        assert_eq!(value["restart_required"], false);
        let versions = registry(&database.0);
        for stored in &versions {
            for key in [public, secret, raw] {
                assert!(!stored.contains(key));
                assert!(!String::from_utf8_lossy(&rotated.stderr).contains(key));
            }
        }
        let current: serde_json::Value = serde_json::from_str(versions.last().unwrap()).unwrap();
        assert_eq!(current.as_object().unwrap().len(), 1);
        assert_eq!(current["projects"][0]["keys"].as_array().unwrap().len(), 2);
    }
    let before = registry(&database.0);
    let repeated = run_in(db, &["init"]);
    assert!(!repeated.status.success());
    assert!(repeated.stdout.is_empty());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("already initialized"));
    assert_eq!(registry(&database.0), before);
}

#[test]
fn commands_fail_clearly_before_initialization_or_without_clickhouse() {
    let database = Database(database());
    let output = run_in(Some(&database.0), &["project", "list"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("run init first"));
    let mut command = Command::new(BINARY);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("CHUNKIFY_")) {
        command.env_remove(key);
    }
    let output = command
        .args(["project", "list"])
        .env("CHUNKIFY_CLICKHOUSE_URL", "http://127.0.0.1:1")
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ClickHouse"));
}

#[test]
fn project_creation_generates_unique_ids_and_preserves_the_display_name() {
    let database = Database(database());
    let db = Some(database.0.as_str());
    assert!(run_in(db, &["init"]).status.success());
    let mut ids = Vec::new();
    for _ in 0..2 {
        let output = run_in(db, &["project", "create", "Production / Europe"]);
        assert!(output.status.success(), "{output:?}");
        let project: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let id = project["project_id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert_eq!(project["name"], "Production / Europe");
        ids.push(id.to_owned());
    }
    assert_ne!(ids[0], ids[1]);
    let output = run_in(db, &["project", "list"]);
    assert!(output.status.success(), "{output:?}");
    let projects: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for id in ids {
        let project = projects
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap();
        assert_eq!(project["name"], "Production / Europe");
    }
}

#[test]
fn private_key_access_defaults_to_full_and_read_only_survives_rotation() {
    let database = Database(database());
    let db = Some(database.0.as_str());
    let command = |args: &[&str]| {
        let output = run_in(db, args);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    assert!(run_in(db, &["init"]).status.success());
    let original = command(&["project", "list"]);
    assert!(
        original[0]["keys"]
            .as_array()
            .unwrap()
            .iter()
            .all(|key| key["read_only"] == false)
    );
    let full = command(&["key", "create", "default", "secret", "Administration"]);
    assert_eq!(full["read_only"], false);
    let readonly = command(&["key", "create", "default", "secret", "MCP", "--read-only"]);
    assert_eq!(readonly["read_only"], true);
    assert_eq!(
        readonly["restart_required"], false,
        "running servers apply key changes live"
    );
    let rotated = command(&[
        "key",
        "rotate",
        "default",
        readonly["key_id"].as_str().unwrap(),
    ]);
    assert_eq!(rotated["key_id"], readonly["key_id"]);
    assert_eq!(rotated["read_only"], true);
    assert_ne!(rotated["key"], readonly["key"]);
    let listing = command(&["project", "list"]);
    let keys = listing[0]["keys"].as_array().unwrap();
    assert_eq!(
        keys.iter().find(|key| key["id"] == full["key_id"]).unwrap()["read_only"],
        false
    );
    assert_eq!(
        keys.iter()
            .find(|key| key["id"] == readonly["key_id"])
            .unwrap()["read_only"],
        true
    );
    let before = registry(&database.0);
    let rejected = run_in(
        db,
        &[
            "key",
            "create",
            "default",
            "public",
            "Invalid",
            "--read-only",
        ],
    );
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("Only secret keys support read-only access")
    );
    assert_eq!(registry(&database.0), before);
    let configured = run_in(
        db,
        &[
            "project",
            "configure",
            "default",
            "/nonexistent-settings.json",
        ],
    );
    assert!(!configured.status.success());
}
