//! `artifact-mcp web-push-keys` prints a key pair that the server's own parser accepts.

use std::process::Command;

use artifact_mcp::config::vapid_public_key;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

const PRIVATE_PREFIX: &str = "WEB_PUSH_VAPID_PRIVATE_KEY=";
const PUBLIC_PREFIX: &str = "# Public key (applicationServerKey, derived by the server): ";

fn generate() -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_artifact-mcp"))
        .env_clear()
        .arg("web-push-keys")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.ends_with('\n'));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    let private_key = lines[0].strip_prefix(PRIVATE_PREFIX).unwrap().to_owned();
    let public_key = lines[1].strip_prefix(PUBLIC_PREFIX).unwrap().to_owned();
    (private_key, public_key)
}

#[test]
fn printed_key_pair_is_accepted_and_derived_by_the_server() {
    let (private_key, public_key) = generate();
    assert_eq!(URL_SAFE_NO_PAD.decode(&private_key).unwrap().len(), 32);
    let point = URL_SAFE_NO_PAD.decode(&public_key).unwrap();
    assert_eq!(point.len(), 65);
    assert_eq!(point[0], 0x04);
    assert_eq!(
        vapid_public_key(&private_key).as_deref(),
        Some(public_key.as_str())
    );
    let (second, _) = generate();
    assert_ne!(private_key, second, "every run draws a new key");
}

#[test]
fn web_push_keys_takes_no_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_artifact-mcp"))
        .env_clear()
        .args(["web-push-keys", "extra"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
