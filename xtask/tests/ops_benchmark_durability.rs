//! `xtask benchmark-durability` without a Docker daemon: a fake `docker` on `PATH` answers
//! `image inspect`, `run`, `port` and `rm`, and its `port` points at an in-process mock MCP
//! server, so the whole image-mode flow (identity, container arguments, health polling, both
//! scenarios, report, cleanup) runs for real.
//!
//! * `XTASK_BENCH_SERVER_BINARY=PATH` also benchmarks a real `artifact-mcp` binary through
//!   `--binary` (for example `target/release/artifact-mcp`).
//! * `XTASK_NODE_PARITY_DIR=DIR` (holding the former `benchmark-durability-oci.mjs`) runs the
//!   image flow through Node against the same fakes and compares Docker arguments, JSON-RPC
//!   bodies and the report shape.

#[path = "ops_support/mock_http.rs"]
mod mock_http;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mock_http::{MockServer, Recorded, Reply};
use serde_json::{Value, json};

const TOKEN: &str = "artifact-mcp-benchmark-token";
/// What the port passes beyond the former script: the audit-ledger key a current server needs
/// and the raised ingress budgets a benchmark-sized run needs.
const EXTRA_ENV: &str = "--env AUDIT_LEDGER_HMAC_KEY=AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE= \
--env INGRESS_MCP_PER_WINDOW=1000000 --env INGRESS_MUTATIONS_PER_WINDOW=1000000 \
--env INGRESS_UPLOADS_PER_WINDOW=1000000";

const FAKE_DOCKER: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_DOCKER_LOG"
case "$1 $2" in
  "image inspect")
    if [ -n "$FAKE_DOCKER_NO_IMAGE" ]; then
      echo "Error: No such image: $3" >&2
      exit 1
    fi
    printf '%s' '[{"Id":"sha256:1111111111111111111111111111111111111111111111111111111111111111","RepoDigests":["ghcr.io/agentshelf-oss/artifact-mcp@sha256:2222222222222222222222222222222222222222222222222222222222222222"],"Config":{"Labels":{"maintainer":"x","org.opencontainers.image.revision":"abc123","org.opencontainers.image.version":"1.26.0"}}}]'
    ;;
  "port "*) echo "0.0.0.0:1"; echo "127.0.0.1:$FAKE_MCP_PORT" ;;
  "run "*) echo "0123456789abcdef" ;;
esac
"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        use std::hash::{BuildHasher, RandomState};
        let suffix = RandomState::new().hash_one((label, std::process::id()));
        let path = std::env::temp_dir().join(format!("xtask-bench-{label}-{suffix:x}"));
        std::fs::create_dir_all(path.join("bin")).unwrap();
        let docker = path.join("bin/docker");
        std::fs::write(&docker, FAKE_DOCKER).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self(path)
    }

    fn docker_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.0.join("docker.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn report(&self) -> (String, Value) {
        let text = std::fs::read_to_string(self.0.join("report.json")).unwrap();
        let value = serde_json::from_str(&text).unwrap();
        (text, value)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A mock Artifact MCP server: healthy, and every publish returns a fresh id.
fn mock_server() -> MockServer {
    let next = Arc::new(AtomicU64::new(0));
    MockServer::start(move |request: &Recorded| {
        if request.method == "GET" && request.target == "/health" {
            return Reply::raw(200, "text/plain", "ok");
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let name = body["params"]["name"].as_str().unwrap_or_default();
        let id = body["id"].clone();
        let content = match name {
            "publish_artifact" | "publish_bundle" => {
                json!({"id": format!("art-{}", next.fetch_add(1, Ordering::SeqCst)), "revision": 1})
            }
            "update_artifact" => json!({"id": body["params"]["arguments"]["id"], "revision": 2}),
            _ => {
                return Reply::json(
                    200,
                    &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601}}),
                );
            }
        };
        Reply::json(
            200,
            &json!({"jsonrpc": "2.0", "id": id, "result": {"structuredContent": content}}),
        )
    })
}

fn command(program: &Path, scratch: &Scratch, server: &MockServer) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", scratch.0.join("bin").display()),
        )
        .env("FAKE_DOCKER_LOG", scratch.0.join("docker.log"))
        .env("FAKE_MCP_PORT", server.port().to_string())
        .env("BENCH_WARMUPS", "1")
        .env("BENCH_MEASURED", "3")
        .current_dir(&scratch.0);
    command
}

fn xtask(scratch: &Scratch, server: &MockServer) -> Command {
    let mut command = command(Path::new(env!("CARGO_BIN_EXE_xtask")), scratch, server);
    command.arg("benchmark-durability");
    command
}

fn text(output: &[u8]) -> String {
    String::from_utf8(output.to_vec()).unwrap()
}

/// `tool name` per JSON-RPC request, in order.
fn tool_calls(requests: &[Recorded]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| {
            let body = request.json();
            body["params"]["name"].as_str().unwrap().to_owned()
        })
        .collect()
}

fn expected_tool_calls(warmups: usize, measured: usize) -> Vec<String> {
    let mut calls = Vec::new();
    for publish in ["publish_artifact", "publish_bundle"] {
        for _ in 0..warmups + measured {
            calls.push(publish.to_owned());
            calls.push("update_artifact".to_owned());
        }
    }
    calls
}

fn data_dir_from(docker_log: &[String]) -> PathBuf {
    let run = docker_log
        .iter()
        .find(|line| line.starts_with("run "))
        .unwrap();
    let volume = run
        .split(' ')
        .skip_while(|arg| *arg != "--volume")
        .nth(1)
        .unwrap();
    PathBuf::from(volume.strip_suffix(":/data-rust").unwrap())
}

#[test]
fn image_mode_runs_both_scenarios_and_writes_the_report() {
    let scratch = Scratch::new("image");
    let server = mock_server();
    let output: Output = xtask(&scratch, &server)
        .args(["registry.example/artifact-mcp@sha256:abc", "report.json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "production OCI durability benchmark passed: 2 scenarios\n"
    );
    assert!(text(&output.stderr).starts_with(
        "SMOKE ONLY: set BENCH_DATA_DIR and BENCH_REQUIRE_PERSISTENT=1 for persistent-volume evidence.\n"
    ));

    let log = scratch.docker_log();
    let data_dir = data_dir_from(&log);
    let container = log[1].split(' ').nth(4).unwrap().to_owned();
    assert!(container.starts_with("artifact-mcp-durability-bench-"));
    assert_eq!(
        log,
        [
            "image inspect registry.example/artifact-mcp@sha256:abc".to_owned(),
            format!(
                "run --detach --rm --name {container} --publish 127.0.0.1::3480 --volume {}:/data-rust --env ARTIFACT_API_KEYS=bench:default:{TOKEN} --env WEBHOOK_ENC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= {EXTRA_ENV} registry.example/artifact-mcp@sha256:abc",
                data_dir.display()
            ),
            format!("port {container} 3480/tcp"),
            format!("rm --force {container}"),
        ]
    );
    assert!(
        !data_dir.exists(),
        "the temporary data directory is removed"
    );

    let requests = server.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/health");
    assert_eq!(tool_calls(&requests), expected_tool_calls(1, 3));
    for (index, request) in requests.iter().skip(1).enumerate() {
        assert_eq!(request.target, "/mcp");
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.json()["id"], index + 1);
    }
    let first_update = requests[2].json();
    assert_eq!(first_update["params"]["arguments"]["id"], "art-0");
    let html = first_update["params"]["arguments"]["html"]
        .as_str()
        .unwrap();
    assert_eq!(
        html.len(),
        "<main>".len() + 64 * 1024 + "updated-warmup-0</main>".len()
    );
    let bundle = requests
        .iter()
        .skip(1)
        .find(|request| request.json()["params"]["name"] == "publish_bundle")
        .unwrap()
        .json();
    let files = bundle["params"]["arguments"]["files"].as_object().unwrap();
    assert_eq!(files.len(), 10);
    assert!(
        files["9.html"]
            .as_str()
            .unwrap()
            .ends_with("warmup-0-9</main>")
    );

    let (raw, report) = scratch.report();
    assert!(raw.ends_with("}\n") && !raw.contains(TOKEN));
    let order: Vec<usize> = [
        "\"schemaVersion\"",
        "\"kind\"",
        "\"requestedImage\"",
        "\"environment\"",
        "\"serverOverrides\"",
        "\"scenarios\"",
        "\"image\"",
        "\"completedAt\"",
    ]
    .iter()
    .map(|key| raw.find(key).unwrap_or_else(|| panic!("{key} missing")))
    .collect();
    assert!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "report key order"
    );
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(
        report["serverOverrides"],
        json!({
            "INGRESS_MCP_PER_WINDOW": "1000000",
            "INGRESS_MUTATIONS_PER_WINDOW": "1000000",
            "INGRESS_UPLOADS_PER_WINDOW": "1000000"
        })
    );
    for credential in ["AQEBAQEB", "AAAAAAAAAAAA", "bench:default"] {
        assert!(!raw.contains(credential), "{credential} reached the report");
    }
    assert_eq!(
        report["kind"],
        "artifact-mcp-production-oci-durability-benchmark"
    );
    assert_eq!(
        report["image"],
        json!({
            "id": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "repoDigests": ["ghcr.io/agentshelf-oss/artifact-mcp@sha256:2222222222222222222222222222222222222222222222222222222222222222"],
            "labels": {"org.opencontainers.image.revision": "abc123", "org.opencontainers.image.version": "1.26.0"}
        })
    );
    let environment = report["environment"].as_object().unwrap();
    for key in [
        "host",
        "kernel",
        "cpu",
        "cpus",
        "dataDir",
        "persistentEvidence",
        "mount",
        "statfs",
    ] {
        assert!(environment.contains_key(key), "{key}");
    }
    assert_eq!(environment["persistentEvidence"], false);
    assert!(environment["statfs"]["type"].as_u64().is_some());
    assert!(environment["statfs"]["blockSize"].as_u64().unwrap() > 0);
    let scenarios = report["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 2);
    for (scenario, name) in scenarios.iter().zip(["single-64KiB", "bundle-10x64KiB"]) {
        assert_eq!(scenario["scenario"], name);
        assert_eq!(scenario["warmups"], 1);
        assert_eq!(scenario["measured"], 3);
        for phase in ["publish", "update"] {
            let summary = &scenario[phase];
            assert_eq!(summary["count"], 3);
            let p50 = summary["p50Ms"].as_f64().unwrap();
            let p95 = summary["p95Ms"].as_f64().unwrap();
            let max = summary["maxMs"].as_f64().unwrap();
            assert!(0.0 < p50 && p50 <= p95 && p95 <= max);
        }
    }
    let completed = report["completedAt"].as_str().unwrap();
    assert_eq!(completed.len(), 24);
    assert!(completed.ends_with('Z') && completed.as_bytes()[10] == b'T');
    assert!(report.get("error").is_none());
}

#[test]
fn a_missing_image_writes_an_error_report_and_still_cleans_up() {
    let scratch = Scratch::new("missing");
    let server = mock_server();
    let output = xtask(&scratch, &server)
        .env("FAKE_DOCKER_NO_IMAGE", "1")
        .args(["missing:tag", "report.json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.ends_with(
        "Error: No such image: missing:tag\nError: Command failed: docker image inspect missing:tag\nError: No such image: missing:tag\n\n"
    ), "{stderr}");
    let log = scratch.docker_log();
    assert_eq!(log[0], "image inspect missing:tag");
    assert!(log[1].starts_with("rm --force artifact-mcp-durability-bench-"));
    assert!(server.requests().is_empty());
    let (_, report) = scratch.report();
    assert_eq!(
        report["error"],
        "Error: Command failed: docker image inspect missing:tag\nError: No such image: missing:tag\n"
    );
    assert!(report.get("image").is_none() && report.get("completedAt").is_none());
    assert_eq!(report["scenarios"], json!([]));
}

#[test]
fn inputs_are_checked_before_docker_runs() {
    let scratch = Scratch::new("inputs");
    let server = mock_server();
    let run = |envs: &[(&str, &str)], args: &[&str]| {
        let output = xtask(&scratch, &server)
            .envs(envs.iter().copied())
            .args(args)
            .output()
            .unwrap();
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr),
        )
    };
    let (code, _, stderr) = run(&[], &["only-image"]);
    assert_eq!(code, Some(1));
    assert!(stderr.starts_with(
        "Error: usage: cargo xtask benchmark-durability IMAGE_OR_LOCAL_TAG OUT.json\n"
    ));

    let (code, stdout, _) = run(&[], &["--help"]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("--binary PATH OUT.json"));

    for (name, value) in [
        ("BENCH_WARMUPS", "-1"),
        ("BENCH_MEASURED", "2.5"),
        ("BENCH_MEASURED", "ten"),
    ] {
        let (code, _, stderr) = run(&[(name, value)], &["img", "report.json"]);
        assert_eq!(code, Some(1));
        assert_eq!(
            stderr,
            format!("Error: {name} must be a non-negative integer\n")
        );
    }

    let busy = scratch.0.join("busy");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("left-over"), "x").unwrap();
    let (code, _, stderr) = run(
        &[("BENCH_DATA_DIR", busy.to_str().unwrap())],
        &["img", "report.json"],
    );
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        format!(
            "Error: BENCH_DATA_DIR must be an empty dedicated directory: {}\n",
            busy.display()
        )
    );

    let (code, _, stderr) = run(
        &[("BENCH_REQUIRE_PERSISTENT", "1")],
        &["img", "report.json"],
    );
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "Error: persistent evidence requires BENCH_DATA_DIR and a discoverable findmnt target\n"
    );
    assert!(
        scratch.docker_log().is_empty(),
        "no Docker call before the inputs are valid"
    );
    assert!(!scratch.0.join("report.json").exists());
}

#[test]
fn a_dedicated_data_dir_is_kept_and_reported() {
    let scratch = Scratch::new("dedicated");
    let server = mock_server();
    let data = scratch.0.join("data");
    let output = xtask(&scratch, &server)
        .env("BENCH_DATA_DIR", &data)
        .env("BENCH_WARMUPS", "0")
        .env("BENCH_MEASURED", "0")
        .args(["img", "report.json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(data.is_dir(), "BENCH_DATA_DIR is never removed");
    let (raw, report) = scratch.report();
    assert_eq!(report["environment"]["dataDir"], data.display().to_string());
    assert!(
        raw.contains("\"publish\": {\n        \"count\": 0\n      }"),
        "{raw}"
    );
}

/// The real server binary, when one is supplied.
#[test]
fn binary_mode_against_a_real_server() {
    let Some(binary) = std::env::var_os("XTASK_BENCH_SERVER_BINARY") else {
        eprintln!("skipped: set XTASK_BENCH_SERVER_BINARY to bench a real artifact-mcp binary");
        return;
    };
    let scratch = Scratch::new("binary");
    let server = mock_server();
    let output = xtask(&scratch, &server)
        .env("BENCH_WARMUPS", "2")
        .env("BENCH_MEASURED", "5")
        .arg("--binary")
        .arg(&binary)
        .arg("report.json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "local binary durability benchmark passed: 2 scenarios\n"
    );
    assert!(server.requests().is_empty() && scratch.docker_log().is_empty());
    let (raw, report) = scratch.report();
    assert!(!raw.contains(TOKEN));
    assert_eq!(
        report["kind"],
        "artifact-mcp-local-binary-durability-benchmark"
    );
    assert_eq!(report["binary"]["sha256"].as_str().unwrap().len(), 64);
    for scenario in report["scenarios"].as_array().unwrap() {
        assert_eq!(scenario["publish"]["count"], 5);
        assert_eq!(scenario["update"]["count"], 5);
    }
    eprintln!("{raw}");
}

#[test]
fn node_parity_when_the_former_script_is_available() {
    let Some(dir) = std::env::var_os("XTASK_NODE_PARITY_DIR") else {
        eprintln!(
            "skipped: set XTASK_NODE_PARITY_DIR to compare with benchmark-durability-oci.mjs"
        );
        return;
    };
    let script = Path::new(&dir).join("benchmark-durability-oci.mjs");
    assert!(script.is_file(), "{} is missing", script.display());
    let node = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("node"))
        .find(|candidate| candidate.is_file())
        .expect("node on PATH");

    let ours = Scratch::new("parity-port");
    let our_server = mock_server();
    let port = xtask(&ours, &our_server)
        .args(["img:tag", "report.json"])
        .output()
        .unwrap();
    let theirs = Scratch::new("parity-node");
    let their_server = mock_server();
    let node_out = command(&node, &theirs, &their_server)
        .arg(&script)
        .args(["img:tag", "report.json"])
        .output()
        .unwrap();
    assert_eq!(port.status.code(), node_out.status.code());
    assert_eq!(port.stdout, node_out.stdout);
    assert_eq!(port.stderr, node_out.stderr);

    // Docker arguments, after the per-process container name and data directory, and without
    // the variables the script predates (`EXTRA_ENV`).
    let normalise = |scratch: &Scratch| -> Vec<String> {
        let log = scratch.docker_log();
        let data = data_dir_from(&log).display().to_string();
        log.iter()
            .map(|line| {
                line.replace(&data, "<DATA>")
                    .replace(&format!(" {EXTRA_ENV}"), "")
                    .split(' ')
                    .map(|arg| {
                        if arg.starts_with("artifact-mcp-durability-bench-") {
                            "<NAME>"
                        } else {
                            arg
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    };
    assert_eq!(normalise(&ours), normalise(&theirs));

    // JSON-RPC bodies byte for byte.
    let bodies = |server: &MockServer| -> Vec<(String, String, Vec<u8>)> {
        server
            .requests()
            .into_iter()
            .map(|request| (request.method, request.target, request.body))
            .collect()
    };
    let (our_bodies, their_bodies) = (bodies(&our_server), bodies(&their_server));
    assert_eq!(our_bodies.len(), their_bodies.len());
    assert!(our_bodies == their_bodies, "request bodies differ");

    // Same report shape and environment; timings and per-run paths differ.
    let mask = |mut report: Value| {
        report.as_object_mut().unwrap().remove("serverOverrides");
        report["completedAt"] = Value::Null;
        report["environment"]["dataDir"] = Value::Null;
        report["environment"]["mount"] = Value::Null;
        for scenario in report["scenarios"].as_array_mut().unwrap() {
            for phase in ["publish", "update"] {
                for key in ["p50Ms", "p95Ms", "maxMs"] {
                    assert!(scenario[phase][key].as_f64().is_some());
                    scenario[phase][key] = Value::Null;
                }
            }
        }
        report
    };
    let (our_raw, our_report) = ours.report();
    let (their_raw, their_report) = theirs.report();
    assert_eq!(mask(our_report), mask(their_report));
    let keys = |raw: &str| -> Vec<String> {
        raw.lines()
            .filter_map(|line| {
                line.trim_start()
                    .strip_prefix('"')
                    .and_then(|rest| rest.split_once('"'))
                    .map(|(key, _)| key.to_owned())
            })
            .filter(|key| key != "serverOverrides" && !key.starts_with("INGRESS_"))
            .collect()
    };
    assert_eq!(keys(&our_raw), keys(&their_raw), "report key order");
    eprintln!(
        "node parity ok: benchmark image flow ({} requests)",
        our_bodies.len()
    );
}
