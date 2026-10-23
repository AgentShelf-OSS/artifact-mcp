//! Ported from the former `test/anthropic-tunnel.test.js` (its three cases first, in order),
//! followed by command-level cases with an injected Docker and a mock Messages API.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

use super::*;
use crate::ops::mock_http::{MockServer, Recorded, Reply};

/// A scratch directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        use std::hash::{BuildHasher, RandomState};
        let suffix = RandomState::new().hash_one((label, std::process::id()));
        let path = std::env::temp_dir().join(format!("xtask-tunnel-{label}-{suffix:x}"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ok_spawn(stdout: &str) -> SpawnResult {
    SpawnResult {
        status: Some(0),
        stdout: stdout.to_owned(),
        error: false,
    }
}

fn tool_round_trip(is_error: bool) -> Value {
    json!({
        "content": [
            {
                "type": "mcp_tool_use",
                "id": "mcptoolu_test",
                "server_name": "artifact-mcp-private",
                "name": "list_artifacts",
                "input": {}
            },
            {
                "type": "mcp_tool_result",
                "tool_use_id": "mcptoolu_test",
                "is_error": is_error,
                "content": []
            }
        ]
    })
}

fn validation_env() -> HashMap<&'static str, String> {
    HashMap::from([
        (
            "ANTHROPIC_TUNNEL_MCP_URL",
            "https://artifacts.example.tunnel.anthropic.com/mcp".to_owned(),
        ),
        ("ANTHROPIC_TUNNEL_TEST_MODEL", "test-model".to_owned()),
        ("ANTHROPIC_API_KEY_FILE", "/secure/anthropic".to_owned()),
        (
            "ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE",
            "/secure/upstream".to_owned(),
        ),
    ])
}

fn secrets(path: &str) -> Result<String, String> {
    match path {
        "/secure/anthropic" => Ok("api-secret".to_owned()),
        "/secure/upstream" => Ok("publisher-secret".to_owned()),
        other => Err(format!("unexpected secret path {other}")),
    }
}

// JS: "default, tunneled, degraded, and rollback deployment paths stay distinct"
#[test]
fn default_tunneled_degraded_and_rollback_deployment_paths_stay_distinct() {
    let project = Scratch::new("compose");
    std::fs::copy(
        crate::repo_root().join("docker-compose.yml"),
        project.0.join("docker-compose.yml"),
    )
    .unwrap();
    std::fs::write(project.0.join(".env"), "ARTIFACT_API_KEYS=\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            project.0.join(".env"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let render = |profile: Option<&str>| -> Option<Value> {
        let mut args = vec!["compose"];
        if let Some(profile) = profile {
            args.extend(["--profile", profile]);
        }
        args.extend(["config", "--format", "json"]);
        let output = match Command::new("docker")
            .args(&args)
            .current_dir(&project.0)
            .env("COMPOSE_PROJECT_NAME", "artifact-mcp-tunnel-test")
            .output()
        {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            other => other.expect("run docker compose config"),
        };
        assert!(
            output.status.success(),
            "Compose configuration must render: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(serde_json::from_slice(&output.stdout).unwrap())
    };

    if let Some(default_config) = render(None) {
        let services: Vec<&String> = default_config["services"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        assert_eq!(services, ["artifact-mcp"]);
        assert_eq!(
            default_config["services"]["artifact-mcp"]["ports"][0],
            json!({
                "mode": "ingress",
                "target": 3480,
                "published": "3480",
                "protocol": "tcp",
                "host_ip": "127.0.0.1"
            })
        );

        let tunneled = render(Some("anthropic-tunnel")).unwrap();
        let proxy = &tunneled["services"]["anthropic-mcp-proxy"];
        let cloudflared = &tunneled["services"]["anthropic-mcp-cloudflared"];
        assert!(proxy.is_object());
        assert!(cloudflared.is_object());
        assert!(proxy.get("ports").is_none());
        assert!(cloudflared.get("ports").is_none());
        assert_eq!(proxy["read_only"], json!(true));
        assert_eq!(proxy["cap_drop"], json!(["ALL"]));
        for image in [&proxy["image"], &cloudflared["image"]] {
            let image = image.as_str().unwrap();
            let digest = image.rsplit_once("@sha256:").map(|(_, digest)| digest);
            assert!(
                digest.is_some_and(|digest| digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
                "{image} must be pinned by digest"
            );
        }
        assert!(cloudflared.get("environment").is_none());
        assert_eq!(
            cloudflared["secrets"],
            json!([{
                "source": "anthropic_tunnel_token",
                "target": "/run/secrets/anthropic_tunnel_token"
            }])
        );
        assert!(
            cloudflared["command"]
                .as_array()
                .unwrap()
                .contains(&json!("--token-file"))
        );
        assert!(!cloudflared.to_string().contains("TUNNEL_TOKEN="));
    } else {
        eprintln!("docker is not installed; skipped the Compose rendering half of this case");
    }

    let health = local_health_plan();
    assert_eq!(health.len(), 3);
    assert!(health[0].name.contains("application health"));
    assert!(health[2].name.contains("tunnel edge connectivity"));

    let mut calls: Vec<Vec<String>> = Vec::new();
    let error = check_local_health(
        &mut |args: &[String]| {
            calls.push(args.to_vec());
            if args.iter().any(|arg| arg == "anthropic-mcp-cloudflared") {
                // A real failure would print this on stderr; it must not reach the message.
                SpawnResult {
                    status: Some(1),
                    stdout: "credential-that-must-not-leak".to_owned(),
                    error: false,
                }
            } else {
                ok_spawn("running")
            }
        },
        &mut |_| {},
    )
    .expect_err("the edge check fails");
    assert!(error.contains("outbound tunnel edge connectivity failed"));
    assert!(!error.contains("credential-that-must-not-leak"));
    assert_eq!(calls.len(), 3);

    let rollback = operation_plan("rollback").unwrap();
    assert!(!rollback[0].args.iter().any(|arg| arg == "--profile"));
    assert_eq!(
        rollback[1].args[rollback[1].args.len() - 3..],
        ["stop", "anthropic-mcp-cloudflared", "anthropic-mcp-proxy"]
    );

    let mut executed: Vec<Vec<String>> = Vec::new();
    run_operation(
        "rollback",
        &mut |args: &[String]| {
            executed.push(args.to_vec());
            ok_spawn("")
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        executed,
        rollback
            .iter()
            .map(|step| step.args.clone())
            .collect::<Vec<_>>()
    );
}

// JS: "remote validation is read-only, Anthropic-side, and never logs its credentials"
#[test]
fn remote_validation_is_read_only_anthropic_side_and_never_logs_its_credentials() {
    let server = MockServer::start(|_| Reply::json(200, &tool_round_trip(false)));
    let env = validation_env();
    let lookup = |name: &str| env.get(name).cloned();
    validate_remote_registration(&lookup, &format!("{}/v1/messages", server.base()), &secrets)
        .unwrap();

    assert_eq!(
        ANTHROPIC_MESSAGES_URL,
        "https://api.anthropic.com/v1/messages"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/v1/messages");
    assert_eq!(request.header("x-api-key"), Some("api-secret"));
    assert_eq!(
        request.header("anthropic-beta"),
        Some("mcp-client-2025-11-20")
    );
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    let body = request.json();
    assert_eq!(
        body["mcp_servers"][0]["authorization_token"],
        "publisher-secret"
    );
    assert_eq!(
        body["tools"][0]["default_config"],
        json!({"enabled": false})
    );
    let configs: Vec<&String> = body["tools"][0]["configs"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(configs, ["list_artifacts"]);

    let serialized_plan = format!("{:?}", operation_plan("start").unwrap());
    assert!(!serialized_plan.contains("api-secret"));
    assert!(!serialized_plan.contains("publisher-secret"));
}

// JS: "remote validation rejects unsafe URLs and incomplete tunnel results"
#[test]
fn remote_validation_rejects_unsafe_urls_and_incomplete_tunnel_results() {
    assert!(
        validate_tunnel_url("https://artifacts.example.com/mcp")
            .unwrap_err()
            .contains("tunnel.anthropic.com")
    );
    assert!(
        validate_tunnel_url("https://artifacts.example.tunnel.anthropic.com/not-mcp")
            .unwrap_err()
            .contains("/mcp path")
    );

    let request = build_remote_validation_request(
        "https://artifacts.example.tunnel.anthropic.com/mcp",
        "secret",
        "test-model",
    );
    let request: Value = serde_json::from_str(&js::stringify(&request)).unwrap();
    assert_eq!(
        request["mcp_servers"][0]["url"],
        "https://artifacts.example.tunnel.anthropic.com/mcp"
    );

    let server = MockServer::start(|_| Reply::json(200, &tool_round_trip(true)));
    let env = validation_env();
    let lookup = |name: &str| env.get(name).cloned();
    let error = validate_remote_registration(
        &lookup,
        &format!("{}/v1/messages", server.base()),
        &|_: &str| Ok("secret".to_owned()),
    )
    .unwrap_err();
    assert!(error.contains("did not complete the read-only list_artifacts probe"));
}

#[test]
fn the_validation_request_is_byte_stable() {
    let body = js::stringify(&build_remote_validation_request(
        "https://a.tunnel.anthropic.com/mcp",
        "tok",
        "m",
    ));
    assert_eq!(
        body,
        r#"{"model":"m","max_tokens":256,"messages":[{"role":"user","content":"Call list_artifacts exactly once with an empty input, then say validation complete."}],"mcp_servers":[{"type":"url","url":"https://a.tunnel.anthropic.com/mcp","name":"artifact-mcp-private","authorization_token":"tok"}],"tools":[{"type":"mcp_toolset","mcp_server_name":"artifact-mcp-private","default_config":{"enabled":false},"configs":{"list_artifacts":{"enabled":true,"defer_loading":false}}}]}"#
    );
}

#[test]
fn tunnel_urls_are_normalised_and_constrained() {
    assert_eq!(
        validate_tunnel_url("HTTPS://X.Tunnel.Anthropic.com:443/mcp/").unwrap(),
        "https://x.tunnel.anthropic.com/mcp/"
    );
    for (url, fragment) in [
        ("not a url", "valid HTTPS URL"),
        ("http://x.tunnel.anthropic.com/mcp", "without credentials"),
        (
            "https://u:p@x.tunnel.anthropic.com/mcp",
            "without credentials",
        ),
        (
            "https://x.tunnel.anthropic.com/mcp?a=1",
            "without credentials",
        ),
        (
            "https://x.tunnel.anthropic.com/mcp#f",
            "without credentials",
        ),
        ("https://tunnel.anthropic.com/mcp", "without credentials"),
        ("https://x.tunnel.anthropic.com/", "/mcp path"),
    ] {
        assert!(
            validate_tunnel_url(url).unwrap_err().contains(fragment),
            "{url}"
        );
    }
}

#[test]
fn remote_validation_failures_name_the_cause_without_credentials() {
    let env = validation_env();
    let lookup = |name: &str| env.get(name).cloned();
    let attempt = |reply: Reply| {
        let server = MockServer::start(move |_| reply.clone());
        validate_remote_registration(&lookup, &format!("{}/v1/messages", server.base()), &secrets)
            .unwrap_err()
    };
    assert_eq!(
        attempt(Reply::json(
            401,
            &json!({"type": "error", "error": {"type": "authentication_error", "message": "api-secret is invalid"}})
        )),
        "Anthropic-side MCP tunnel validation failed with HTTP 401 (authentication_error)"
    );
    assert_eq!(
        attempt(Reply::json(500, &json!({"error": null}))),
        "Anthropic-side MCP tunnel validation failed with HTTP 500 (unknown_error)"
    );
    assert_eq!(
        attempt(Reply::raw(502, "text/html", "<html>api-secret</html>")),
        "Anthropic-side MCP tunnel validation returned non-JSON HTTP 502"
    );
    assert_eq!(
        attempt(Reply::json(
            200,
            &json!({"content": [{"type": "text", "text": "hi"}]})
        )),
        "Anthropic reached the tunnel but did not complete the read-only list_artifacts probe"
    );

    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    assert_eq!(
        validate_remote_registration(&lookup, &format!("http://{closed}/v1/messages"), &secrets)
            .unwrap_err(),
        "Anthropic-side MCP tunnel validation could not reach the Messages API"
    );

    let missing = |name: &str| {
        (name != "ANTHROPIC_TUNNEL_TEST_MODEL")
            .then(|| lookup(name))
            .flatten()
    };
    assert_eq!(
        validate_remote_registration(&missing, "http://127.0.0.1:9/", &secrets).unwrap_err(),
        "ANTHROPIC_TUNNEL_TEST_MODEL is required"
    );
}

#[test]
fn secret_files_must_hold_one_line() {
    let dir = Scratch::new("secrets");
    let write = |name: &str, contents: &str| {
        let path = dir.0.join(name);
        std::fs::write(&path, contents).unwrap();
        path.display().to_string()
    };
    assert_eq!(
        read_secret(&write("good", "\u{feff}  token-1 \n")).unwrap(),
        "token-1"
    );
    for contents in ["", " \n ", "a\nb", "a\rb"] {
        let path = write("bad", contents);
        assert_eq!(
            read_secret(&path).unwrap_err(),
            format!("Secret file {path} must contain one non-empty line")
        );
    }
    let path = write("long", &"x".repeat(16_385));
    assert!(read_secret(&path).is_err());
    let missing = dir.0.join("missing").display().to_string();
    assert_eq!(
        read_secret(&missing).unwrap_err(),
        format!("ENOENT: no such file or directory, open '{missing}'")
    );
}

struct Cli {
    ok: bool,
    stdout: String,
    stderr: String,
    docker: Vec<Vec<String>>,
}

/// Run the command with Docker answered by `docker` and the Messages API at `messages_url`.
fn cli(
    args: &[&str],
    docker: impl Fn(&[String]) -> SpawnResult,
    env: &dyn Fn(&str) -> Option<String>,
    messages_url: &str,
) -> Cli {
    let calls = RefCell::new(Vec::new());
    let mut spawn = |args: &[String]| {
        calls.borrow_mut().push(args.to_vec());
        docker(args)
    };
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let ok = run(
        &args,
        &mut spawn,
        env,
        messages_url,
        &secrets,
        &mut out,
        &mut err,
    );
    Cli {
        ok,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
        docker: calls.into_inner(),
    }
}

fn healthy(_: &[String]) -> SpawnResult {
    ok_spawn("container-id\n")
}

fn no_env(_: &str) -> Option<String> {
    None
}

fn plan_args(steps: &[Step]) -> Vec<Vec<String>> {
    steps.iter().map(|step| step.args.clone()).collect()
}

#[test]
fn start_runs_the_profile_then_every_health_check() {
    let result = cli(&["start"], healthy, &no_env, "http://127.0.0.1:9/");
    assert!(result.ok);
    let mut expected = plan_args(&operation_plan("start").unwrap());
    expected.extend(plan_args(&local_health_plan()));
    assert_eq!(result.docker, expected);
    assert_eq!(
        result.docker[0].join(" "),
        "compose --profile anthropic-tunnel up -d --wait --wait-timeout 60 artifact-mcp anthropic-mcp-proxy anthropic-mcp-cloudflared"
    );
    assert_eq!(
        result.stdout,
        "[ok] start Artifact MCP and the private tunnel profile
[ok] Artifact MCP application health
[ok] Anthropic MCP proxy process
[ok] outbound tunnel edge connectivity
Tunnel profile is locally healthy. Run `cargo xtask tunnel validate` before cutover.
"
    );
    assert_eq!(result.stderr, "");
}

#[test]
fn status_reports_the_first_failing_signal() {
    let result = cli(
        &["status"],
        |args: &[String]| {
            if args.contains(&"ps".to_owned()) {
                ok_spawn("  \n")
            } else {
                healthy(args)
            }
        },
        &no_env,
        "http://127.0.0.1:9/",
    );
    assert!(!result.ok);
    assert_eq!(result.docker.len(), 2);
    assert_eq!(result.stdout, "[ok] Artifact MCP application health\n");
    assert_eq!(
        result.stderr,
        "Anthropic MCP proxy process failed (exit 0)\n"
    );

    let result = cli(
        &["status"],
        |_: &[String]| SpawnResult {
            error: true,
            ..SpawnResult::default()
        },
        &no_env,
        "http://127.0.0.1:9/",
    );
    assert_eq!(
        result.stderr,
        "Artifact MCP application health failed (could not run Docker)\n"
    );

    let result = cli(
        &["status"],
        |_: &[String]| SpawnResult::default(),
        &no_env,
        "http://127.0.0.1:9/",
    );
    assert_eq!(
        result.stderr,
        "Artifact MCP application health failed (exit unknown)\n"
    );
}

#[test]
fn rollback_restores_the_default_service_and_checks_it() {
    let result = cli(&["rollback"], healthy, &no_env, "http://127.0.0.1:9/");
    assert!(result.ok);
    let mut expected = plan_args(&operation_plan("rollback").unwrap());
    expected.push(local_health_plan()[0].args.clone());
    assert_eq!(result.docker, expected);
    assert_eq!(
        result.stdout,
        "[ok] restore the default Artifact MCP service
[ok] stop the private tunnel profile
[ok] Artifact MCP application health
Default deployment restored. Re-enable the former public /mcp edge policy if it was removed after cutover.
"
    );
}

#[test]
fn validate_checks_locally_then_calls_the_messages_api() {
    let server = MockServer::start(|_| Reply::json(200, &tool_round_trip(false)));
    let env = validation_env();
    let lookup = |name: &str| env.get(name).cloned();
    let url = format!("{}/v1/messages", server.base());
    let result = cli(&["validate"], healthy, &lookup, &url);
    assert!(result.ok, "{}", result.stderr);
    assert_eq!(result.docker, plan_args(&local_health_plan()));
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        result.stdout,
        "[ok] Artifact MCP application health
[ok] Anthropic MCP proxy process
[ok] outbound tunnel edge connectivity
[ok] Anthropic-side registration, routing, authentication, and read-only tool call
"
    );
    for secret in ["api-secret", "publisher-secret"] {
        assert!(!result.stdout.contains(secret) && !result.stderr.contains(secret));
    }

    let result = cli(&["validate"], healthy, &no_env, &url);
    assert!(!result.ok);
    assert_eq!(result.stderr, "ANTHROPIC_TUNNEL_MCP_URL is required\n");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn usage_goes_to_stdout_for_help_and_stderr_for_mistakes() {
    for args in [&[][..], &["--help"], &["-h"], &[""]] {
        let result = cli(args, healthy, &no_env, "http://127.0.0.1:9/");
        assert!(result.ok);
        assert!(result.docker.is_empty());
        assert!(
            result
                .stdout
                .starts_with("Usage: cargo xtask tunnel <start|status|validate|rollback>\n")
        );
        assert!(
            result
                .stdout
                .ends_with("  ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE\n")
        );
    }
    for args in [&["restart"][..], &["start", "now"], &["--start"]] {
        let result = cli(args, healthy, &no_env, "http://127.0.0.1:9/");
        assert!(!result.ok);
        assert!(result.docker.is_empty());
        assert_eq!(result.stdout, "");
        assert!(result.stderr.starts_with("Usage: cargo xtask tunnel"));
    }
    assert_eq!(
        operation_plan("status").unwrap_err(),
        "Unsupported tunnel action: status"
    );
}

/// The request the Messages API receives, from the port and from the former Node script.
#[test]
fn node_parity_when_the_former_script_is_available() {
    let Some(dir) = std::env::var_os("XTASK_NODE_PARITY_DIR") else {
        eprintln!("skipped: set XTASK_NODE_PARITY_DIR to compare with anthropic-tunnel.mjs");
        return;
    };
    let script = std::path::Path::new(&dir).join("anthropic-tunnel.mjs");
    assert!(script.is_file(), "{} is missing", script.display());
    let secrets_dir = Scratch::new("parity");
    let api_key = secrets_dir.0.join("anthropic");
    let upstream = secrets_dir.0.join("upstream");
    std::fs::write(&api_key, "api-secret\n").unwrap();
    std::fs::write(&upstream, "publisher-secret\n").unwrap();
    let fake_docker = secrets_dir.0.join("docker");
    std::fs::write(&fake_docker, "#!/bin/sh\necho running\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let env: Vec<(&str, String)> = vec![
        (
            "ANTHROPIC_TUNNEL_MCP_URL",
            "https://artifacts.example.tunnel.anthropic.com/mcp".to_owned(),
        ),
        ("ANTHROPIC_TUNNEL_TEST_MODEL", "test-model".to_owned()),
        ("ANTHROPIC_API_KEY_FILE", api_key.display().to_string()),
        (
            "ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE",
            upstream.display().to_string(),
        ),
    ];
    for reply in [
        Reply::json(200, &tool_round_trip(false)),
        Reply::json(200, &tool_round_trip(true)),
        Reply::json(429, &json!({"error": {"type": "rate_limit_error"}})),
        Reply::raw(502, "text/html", "<html/>"),
    ] {
        let node_reply = reply.clone();
        let node_server = MockServer::start(move |_| node_reply.clone());
        let output = Command::new("node")
            .arg("--import")
            .arg("data:text/javascript,const f=globalThis.fetch;globalThis.fetch=(u,o)=>f(String(u).replace('https://api.anthropic.com',process.env.PARITY_BASE),o);")
            .arg(&script)
            .arg("validate")
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    secrets_dir.0.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("PARITY_BASE", node_server.base())
            .envs(env.iter().map(|(name, value)| (*name, value)))
            .output()
            .expect("run node");

        let rust_server = MockServer::start(move |_| reply.clone());
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        };
        let calls = RefCell::new(0);
        let mut spawn = |_: &[String]| {
            *calls.borrow_mut() += 1;
            ok_spawn("running\n")
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ok = run(
            &["validate".to_owned()],
            &mut spawn,
            &lookup,
            &format!("{}/v1/messages", rust_server.base()),
            &read_secret,
            &mut out,
            &mut err,
        );
        assert_eq!(ok, output.status.success());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            String::from_utf8(output.stdout).unwrap()
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            String::from_utf8(output.stderr).unwrap()
        );
        let ours: Vec<Recorded> = rust_server.requests();
        let theirs: Vec<Recorded> = node_server.requests();
        assert_eq!(ours.len(), theirs.len());
        for (ours, theirs) in ours.iter().zip(&theirs) {
            assert_eq!(ours.target, theirs.target);
            assert_eq!(ours.body, theirs.body, "request bodies differ");
            for header in [
                "content-type",
                "x-api-key",
                "anthropic-version",
                "anthropic-beta",
            ] {
                assert_eq!(ours.header(header), theirs.header(header), "{header}");
            }
        }
        eprintln!("node parity ok: tunnel validate (exit ok = {ok})");
    }
}
