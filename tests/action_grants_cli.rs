use std::{fs, process::Command};

#[test]
fn action_grant_preflight_checks_permissions_without_starting_the_application() {
    let root = std::env::temp_dir().join(format!("action-grants-cli-{}", nanoid::nanoid!()));
    fs::create_dir(&root).unwrap();
    let grants = root.join("grants.json");
    fs::write(&grants, r#"[{"artifact_id":"abc123def456","org":"homelab","revision":1,"action":"investigate-SFD-703","worker_url":"http://127.0.0.1:1/"}]"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_artifact-mcp"))
        .env_clear()
        .env("DATA_DIR", root.join("must-not-be-created"))
        .arg("check-action-grants")
        .arg(&grants)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        r#"{"status":"ok","check":"action-grants"}"#
    );
    assert!(!root.join("must-not-be-created").exists());

    fs::write(&grants, r#"[{"artifact_id":"private-artifact-id","org":"homelab","revision":1,"action":"investigate-SFD-999","worker_url":"http://127.0.0.1:1/"}]"#).unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_artifact-mcp"))
        .env_clear()
        .env("DATA_DIR", root.join("must-not-be-created"))
        .arg("check-action-grants")
        .arg(&grants)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
    assert_eq!(
        String::from_utf8(rejected.stderr).unwrap().trim(),
        "Invalid action grants"
    );
    assert!(!root.join("must-not-be-created").exists());
    fs::remove_dir_all(root).unwrap();
}
