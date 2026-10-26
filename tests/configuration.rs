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
        "COLLECTOR_DATA_DIR=from-dotenv\nCOLLECTOR_RETENTION_SECONDS=invalid\n",
    )
    .unwrap();
    assert!(!fixture.run(&["config", "check"]).status.success());
    let output = fixture
        .command()
        .args(["init"])
        .env("COLLECTOR_RETENTION_SECONDS", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(fixture.0.join("from-dotenv/keys.json").is_file());
}

#[test]
fn an_explicit_env_file_replaces_dotenv_and_cli_overrides_its_data_directory() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join(".env"),
        "COLLECTOR_RETENTION_SECONDS=invalid\n",
    )
    .unwrap();
    std::fs::write(
        fixture.0.join("custom.env"),
        "COLLECTOR_DATA_DIR=unused\nCOLLECTOR_STORE_IP=false\n",
    )
    .unwrap();
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
        "CHUNKIFY_MCP_READ_TOKEN='private-test-secret\n",
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
