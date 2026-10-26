use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_chunkify-analytics");

fn run(args: &[&str]) -> Output {
    let mut command = Command::new(BINARY);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("COLLECTOR_")) {
        command.env_remove(key);
    }
    command.args(args).output().unwrap()
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
            .env("COLLECTOR_BIND", "invalid")
            .env("COLLECTOR_DATA_DIR", "/nonexistent-cli-help-data")
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
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

#[test]
fn flags_override_environment_and_configuration_check_needs_no_data_directory() {
    let output = Command::new(BINARY)
        .args(["serve", "--bind", "127.0.0.1:0", "--help"])
        .env("COLLECTOR_BIND", "invalid")
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = run(&[
        "config",
        "check",
        "--data-dir",
        "/nonexistent-cli-check-data",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Configuration is valid."
    );
}

#[test]
fn data_directory_flag_is_global_and_controls_initialization() {
    let directory = std::env::temp_dir().join(format!("chunkify-cli-{}", uuid::Uuid::new_v4()));
    let output = run(&["init", "--data-dir", directory.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    assert!(directory.join("keys.json").is_file());
    let output = run(&["--data-dir", directory.to_str().unwrap(), "project", "list"]);
    assert!(output.status.success(), "{output:?}");
    let projects: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(projects[0]["id"], "default");
    std::fs::remove_dir_all(directory).unwrap();
}
