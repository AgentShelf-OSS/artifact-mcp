//! `xtask tunnel` as a process, with a fake `docker` first on `PATH` that logs its working
//! directory and arguments. Proves the command runs Compose from the repository root, prints
//! only step names and statuses (never Docker's output), and exits like the former script.
//!
//! With `XTASK_NODE_PARITY_DIR` set to a directory holding the former `anthropic-tunnel.mjs`,
//! the same runs go through Node and must print the same lines (after the renamed invocation)
//! and send Docker the same arguments.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE_DOCKER: &str = r#"#!/bin/sh
printf '%s|%s\n' "$PWD" "$*" >> "$FAKE_DOCKER_LOG"
if [ -n "$FAKE_DOCKER_FAIL" ]; then
  case "$*" in
    *"$FAKE_DOCKER_FAIL"*)
      echo "docker-stdout-must-not-leak"
      echo "docker-stderr-must-not-leak" >&2
      exit 3
      ;;
  esac
fi
echo running
"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        use std::hash::{BuildHasher, RandomState};
        let suffix = RandomState::new().hash_one((label, std::process::id()));
        let path = std::env::temp_dir().join(format!("xtask-tunnel-cli-{label}-{suffix:x}"));
        std::fs::create_dir_all(&path).unwrap();
        let docker = path.join("docker");
        std::fs::write(&docker, FAKE_DOCKER).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self(path)
    }

    fn log(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.0.join("docker.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let (cwd, args) = line.split_once('|').unwrap();
                (cwd.to_owned(), args.to_owned())
            })
            .collect()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
}

fn launch(program: &Path, scratch: &Scratch, fail: &str, path: Option<&str>) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env(
            "PATH",
            path.map_or_else(
                || format!("{}:/usr/bin:/bin", scratch.0.display()),
                str::to_owned,
            ),
        )
        .env("FAKE_DOCKER_LOG", scratch.0.join("docker.log"))
        .env("FAKE_DOCKER_FAIL", fail);
    command
}

fn xtask(scratch: &Scratch, args: &[&str], fail: &str) -> Output {
    launch(Path::new(env!("CARGO_BIN_EXE_xtask")), scratch, fail, None)
        .arg("tunnel")
        .args(args)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

const HEALTH: [&str; 3] = [
    "compose exec -T artifact-mcp /artifact-mcp healthcheck",
    "compose --profile anthropic-tunnel ps --status running -q anthropic-mcp-proxy",
    "compose --profile anthropic-tunnel exec -T anthropic-mcp-cloudflared cloudflared tunnel --metrics 127.0.0.1:2000 ready",
];

#[test]
fn start_runs_compose_from_the_repository_root() {
    let scratch = Scratch::new("start");
    let output = xtask(&scratch, &["start"], "");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "[ok] start Artifact MCP and the private tunnel profile
[ok] Artifact MCP application health
[ok] Anthropic MCP proxy process
[ok] outbound tunnel edge connectivity
Tunnel profile is locally healthy. Run `cargo xtask tunnel validate` before cutover.
"
    );
    assert_eq!(text(&output.stderr), "");
    let log = scratch.log();
    let root = repo_root().display().to_string();
    assert!(log.iter().all(|(cwd, _)| *cwd == root), "{log:?}");
    let args: Vec<&str> = log.iter().map(|(_, args)| args.as_str()).collect();
    let mut expected = vec![
        "compose --profile anthropic-tunnel up -d --wait --wait-timeout 60 artifact-mcp anthropic-mcp-proxy anthropic-mcp-cloudflared",
    ];
    expected.extend(HEALTH);
    assert_eq!(args, expected);
}

#[test]
fn a_failing_check_names_the_step_and_hides_docker_output() {
    let scratch = Scratch::new("status");
    let output = xtask(&scratch, &["status"], "anthropic-mcp-cloudflared");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stdout),
        "[ok] Artifact MCP application health\n[ok] Anthropic MCP proxy process\n"
    );
    assert_eq!(
        text(&output.stderr),
        "outbound tunnel edge connectivity failed (exit 3)\n"
    );
    assert_eq!(scratch.log().len(), 3);

    let missing = xtask_with_path(&scratch, &["status"], "/nonexistent");
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        text(&missing.stderr),
        "Artifact MCP application health failed (could not run Docker)\n"
    );
}

fn xtask_with_path(scratch: &Scratch, args: &[&str], path: &str) -> Output {
    launch(
        Path::new(env!("CARGO_BIN_EXE_xtask")),
        scratch,
        "",
        Some(path),
    )
    .arg("tunnel")
    .args(args)
    .output()
    .unwrap()
}

#[test]
fn rollback_restores_the_default_service() {
    let scratch = Scratch::new("rollback");
    let output = xtask(&scratch, &["rollback"], "");
    assert_eq!(output.status.code(), Some(0));
    let args: Vec<String> = scratch.log().into_iter().map(|(_, args)| args).collect();
    assert_eq!(
        args,
        [
            "compose up -d --wait --wait-timeout 60 artifact-mcp",
            "compose --profile anthropic-tunnel stop anthropic-mcp-cloudflared anthropic-mcp-proxy",
            HEALTH[0],
        ]
    );
    assert!(text(&output.stdout).ends_with(
        "Default deployment restored. Re-enable the former public /mcp edge policy if it was removed after cutover.\n"
    ));
}

#[test]
fn usage_and_exit_codes() {
    let scratch = Scratch::new("usage");
    let help = xtask(&scratch, &[], "");
    assert_eq!(help.status.code(), Some(0));
    assert!(
        text(&help.stdout)
            .starts_with("Usage: cargo xtask tunnel <start|status|validate|rollback>\n")
    );
    let wrong = xtask(&scratch, &["start", "extra"], "");
    assert_eq!(wrong.status.code(), Some(1));
    assert_eq!(text(&wrong.stdout), "");
    assert_eq!(text(&wrong.stderr), text(&help.stdout));
    let validate = xtask(&scratch, &["validate"], "");
    assert_eq!(validate.status.code(), Some(1));
    assert_eq!(
        text(&validate.stderr),
        "ANTHROPIC_TUNNEL_MCP_URL is required\n"
    );
    assert!(
        scratch.log().len() == 3,
        "validate runs the local checks first"
    );
}

#[test]
fn node_parity_when_the_former_script_is_available() {
    let Some(dir) = std::env::var_os("XTASK_NODE_PARITY_DIR") else {
        eprintln!("skipped: set XTASK_NODE_PARITY_DIR to compare with anthropic-tunnel.mjs");
        return;
    };
    let script = Path::new(&dir).join("anthropic-tunnel.mjs");
    assert!(script.is_file(), "{} is missing", script.display());
    let node = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("node"))
        .find(|candidate| candidate.is_file())
        .expect("node on PATH");
    let cases: [(&[&str], &str); 8] = [
        (&["start"], ""),
        (&["start"], "--wait"),
        (&["status"], ""),
        (&["status"], "anthropic-mcp-proxy"),
        (&["rollback"], ""),
        (&["rollback"], "stop"),
        (&["validate"], ""),
        (&["restart"], ""),
    ];
    let normalise = |text: String| {
        text.replace("node scripts/anthropic-tunnel.mjs", "cargo xtask tunnel")
            .replace("npm run tunnel:validate", "cargo xtask tunnel validate")
    };
    for (args, fail) in cases {
        let ours = Scratch::new("parity-port");
        let port = xtask(&ours, args, fail);
        let theirs = Scratch::new("parity-node");
        let node_out = launch(&node, &theirs, fail, None)
            .arg(&script)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            port.status.code(),
            node_out.status.code(),
            "{args:?} {fail}"
        );
        assert_eq!(
            text(&port.stdout),
            normalise(text(&node_out.stdout)),
            "{args:?} {fail}"
        );
        assert_eq!(
            text(&port.stderr),
            normalise(text(&node_out.stderr)),
            "{args:?} {fail}"
        );
        let docker = |scratch: &Scratch| -> Vec<String> {
            scratch.log().into_iter().map(|(_, args)| args).collect()
        };
        assert_eq!(docker(&ours), docker(&theirs), "{args:?} {fail}");
        eprintln!("node parity ok: tunnel {args:?} fail={fail:?}");
    }
}
