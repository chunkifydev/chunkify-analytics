use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_chunkify-analytics");

fn run(args: &[&str]) -> Output {
    let mut command = Command::new(BINARY);
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("COLLECTOR_")) {
        command.env_remove(key);
    }
    let directory = std::env::temp_dir().join(format!("chunkify-cli-cwd-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let output = command.current_dir(&directory).args(args).output().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    output
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

#[test]
fn project_creation_generates_unique_ids_and_preserves_the_display_name() {
    let directory = std::env::temp_dir().join(format!("chunkify-cli-{}", uuid::Uuid::new_v4()));
    let path = directory.to_str().unwrap();
    assert!(run(&["--data-dir", path, "init"]).status.success());
    let mut ids = Vec::new();
    for _ in 0..2 {
        let output = run(&[
            "--data-dir",
            path,
            "project",
            "create",
            "Production / Europe",
        ]);
        assert!(output.status.success(), "{output:?}");
        let project: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let id = project["project_id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert_eq!(project["name"], "Production / Europe");
        ids.push(id.to_owned());
    }
    assert_ne!(ids[0], ids[1]);
    let output = run(&["--data-dir", path, "project", "list"]);
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
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn private_key_access_defaults_to_full_and_read_only_survives_rotation() {
    let directory = std::env::temp_dir().join(format!("chunkify-cli-{}", uuid::Uuid::new_v4()));
    let path = directory.to_str().unwrap();
    let command = |args: &[&str]| {
        let mut arguments = vec!["--data-dir", path];
        arguments.extend_from_slice(args);
        let output = run(&arguments);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    assert!(run(&["--data-dir", path, "init"]).status.success());
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
        readonly["restart_required"], true,
        "offline changes apply on startup"
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

    let filename = directory.join("keys.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&filename).unwrap()).unwrap();
    let keys = saved["projects"][0]["keys"].as_array_mut().unwrap();
    keys.iter_mut()
        .find(|key| key["id"] == full["key_id"])
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("read_only");
    std::fs::write(&filename, serde_json::to_vec(&saved).unwrap()).unwrap();
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
    let rejected = run(&[
        "--data-dir",
        path,
        "key",
        "create",
        "default",
        "public",
        "Invalid",
        "--read-only",
    ]);
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("Only secret keys support read-only access")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(filename).unwrap()).unwrap(),
        saved
    );
    std::fs::remove_dir_all(directory).unwrap();
}
