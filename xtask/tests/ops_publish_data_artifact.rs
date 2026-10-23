//! `xtask publish-data-artifact` exactly as the homelab wrapper runs it: the built binary, the
//! wrapper's minimal environment (`PATH`, `LANG`, `LC_ALL`, `TMPDIR` and the token; no `HOME`
//! or `CARGO_HOME`) and a recording mock MCP server.
//!
//! `fixtures/publish-data-artifact/node-capture.json` holds what the former
//! `scripts/publish-data-artifact.mjs` sent and printed for every scenario against this same
//! mock: JSON-RPC request bodies byte for byte, the headers that matter, stdout, stderr and the
//! exit status. The port must reproduce it exactly.
//!
//! * `XTASK_PUBLISH_BINARY=PATH` runs the scenarios against another build, for example
//!   `target/release/xtask`.
//! * `XTASK_NODE_PARITY_DIR=DIR` (a directory holding the former script, for example from
//!   `git show be16f6b:scripts/publish-data-artifact.mjs`) also runs every scenario through
//!   Node and compares it with the capture; add `XTASK_WRITE_NODE_CAPTURE=1` to rewrite it.

#[path = "ops_support/mock_http.rs"]
mod mock_http;

use std::path::{Path, PathBuf};
use std::process::Command;

use mock_http::{MockServer, Recorded, Reply};
use serde_json::{Value, json};

const TOKEN: &str = "artifact_tok_0123456789abcdefSECRET";
const CAPTURE: &str = "tests/fixtures/publish-data-artifact/node-capture.json";

/// How the mock answers one scenario.
#[derive(Clone, Default)]
struct Mcp {
    /// Answer every request to this JSON-RPC method or tool with this HTTP status.
    http_status: Option<(&'static str, u16)>,
    /// Answer this method or tool with a JSON-RPC error.
    rpc_error: Option<&'static str>,
    /// Answer this tool with `isError: true`.
    tool_error: Option<&'static str>,
    /// Answer this tool with a text content block instead of structured content.
    text_result: Option<&'static str>,
    /// Answer this tool with neither structured content nor text.
    empty_result: Option<&'static str>,
}

impl Mcp {
    fn reply(&self, request: &Recorded) -> Reply {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let id = body["id"].clone();
        let method = body["method"].as_str().unwrap_or_default();
        let name = if method == "tools/call" {
            body["params"]["name"].as_str().unwrap_or_default()
        } else {
            method
        };
        let rpc = |result: Value| {
            Reply::json(200, &json!({"jsonrpc": "2.0", "id": id, "result": result}))
        };
        if let Some((target, status)) = self.http_status
            && target == name
        {
            return if status == 302 {
                Reply::raw(302, "text/plain", "moved")
                    .with_header("Location", "https://elsewhere.example/mcp")
            } else {
                // A body echoing the credential must never reach the output.
                Reply::raw(
                    status,
                    "text/plain",
                    format!(
                        "denied {}",
                        request.header("authorization").unwrap_or_default()
                    ),
                )
            };
        }
        if self.rpc_error == Some(name) {
            return Reply::json(
                200,
                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("bad token {}", request.header("authorization").unwrap_or_default())}}),
            );
        }
        if self.tool_error == Some(name) {
            return rpc(
                json!({"isError": true, "content": [{"type": "text", "text": "binding rejected"}]}),
            );
        }
        let structured = match name {
            "initialize" => {
                return rpc(json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock-artifact-mcp", "version": "0.0.0"}
                }));
            }
            "list_data_sources" => json!({"sources": [
                {"id": "ci", "operations": {"status": {}}, "subscriptions": {}},
                {"id": "github", "operations": {"list_prs": {}, "get_pr": {}}, "subscriptions": {"pr_updates": {}}}
            ]}),
            "publish_artifact" => {
                json!({"id": "a1b2c3d4e5", "url": "https://artifacts.example.com/a/a1b2c3d4e5", "revision": 1})
            }
            "update_artifact" => {
                json!({"id": body["params"]["arguments"]["id"], "url": "https://artifacts.example.com/a/abc123def", "revision": 7})
            }
            "set_data_bindings" => json!({"id": body["params"]["arguments"]["id"], "bindings": 4}),
            _ => {
                return Reply::json(
                    200,
                    &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown"}}),
                );
            }
        };
        if self.empty_result == Some(name) {
            return rpc(json!({"content": [{"type": "image", "data": ""}]}));
        }
        if self.text_result == Some(name) {
            return rpc(json!({"content": [{"type": "text", "text": structured.to_string()}]}));
        }
        rpc(json!({"structuredContent": structured, "content": [{"type": "text", "text": "ok"}]}))
    }
}

const BINDINGS: &str = r#"{
  "bindings": {
    "prs": {"source": "github", "operations": ["list_prs", "get_pr"], "subscriptions": ["pr_updates"]},
    "10": {"source": "ci", "operations": ["status"]},
    "2": {"source": "github", "operations": [], "options": {"z": 1, "a": [1.0, 2.50, 1e3, -0], "b": "é \t"}},
    "checks": {"source": "ci", "subscriptions": [], "prs": {"dup": 1, "dup": 2}}
  },
  "ignored": true
}"#;

/// HTML with escapes, non-ASCII text and one invalid UTF-8 byte (read as U+FFFD).
fn html() -> Vec<u8> {
    let mut html = "<!doctype html><title>PR \"Watch\"</title>\n<script>var s='</scr'+'ipt>\\\\';</script>\t\u{1}é😀\u{2028}".as_bytes().to_vec();
    html.extend([0xff, b'<', b'/', b'p', b'>']);
    html
}

struct Scenario {
    name: &'static str,
    args: Vec<&'static str>,
    mcp: Mcp,
    bindings: &'static str,
    token: Option<&'static str>,
}

fn scenario(name: &'static str, args: &[&'static str]) -> Scenario {
    Scenario {
        name,
        args: args.to_vec(),
        mcp: Mcp::default(),
        bindings: BINDINGS,
        token: Some(TOKEN),
    }
}

const BASE: [&str; 8] = [
    "--mcp",
    "{MCP}",
    "--html",
    "page.html",
    "--bindings",
    "bindings.json",
    "--title",
    "PR Watch",
];

fn base(extra: &[&'static str]) -> Vec<&'static str> {
    let mut args = BASE.to_vec();
    args.extend_from_slice(extra);
    args
}

/// Every scenario whose output Node and the port share.
fn scenarios() -> Vec<Scenario> {
    let mut list = vec![
        scenario(
            "publish",
            &base(&["--description", "Live PRs", "--org", "homelab"]),
        ),
        Scenario {
            mcp: Mcp {
                text_result: Some("update_artifact"),
                ..Mcp::default()
            },
            ..scenario(
                "update",
                &base(&["--artifact-id", "abc123def", "--description=", "--org="]),
            )
        },
        Scenario {
            bindings: r#"{"bindings": {"x": {"source": "jira"}}}"#,
            ..scenario("unavailable-source", &base(&[]))
        },
        Scenario {
            bindings: r#"{"bindings": {"x": {"source": "github", "operations": ["merge_pr"]}}}"#,
            ..scenario("unavailable-operation", &base(&[]))
        },
        Scenario {
            bindings: r#"{"bindings": {"x": {"source": "ci", "operations": ["status"], "subscriptions": ["builds"]}}}"#,
            ..scenario("unavailable-subscription", &base(&[]))
        },
        Scenario {
            bindings: r#"{"bindings": []}"#,
            ..scenario("bindings-not-an-object", &base(&[]))
        },
        Scenario {
            bindings: r#"{"bindings": "#,
            ..scenario("bindings-truncated", &base(&[]))
        },
        Scenario {
            bindings: "null",
            ..scenario("bindings-null", &base(&[]))
        },
        Scenario {
            token: None,
            ..scenario("missing-token", &base(&[]))
        },
        scenario("missing-title", &BASE[..6]),
        scenario(
            "bad-url",
            &[
                "--mcp",
                "not a url",
                "--html",
                "page.html",
                "--bindings",
                "bindings.json",
                "--title",
                "T",
            ],
        ),
        scenario(
            "url-with-credentials",
            &[
                "--mcp",
                "https://user:pw@artifacts.example.com/mcp",
                "--html",
                "page.html",
                "--bindings",
                "bindings.json",
                "--title",
                "T",
            ],
        ),
        scenario(
            "url-with-query",
            &[
                "--mcp",
                "https://artifacts.example.com/mcp?key=1",
                "--html",
                "page.html",
                "--bindings",
                "bindings.json",
                "--title",
                "T",
            ],
        ),
        scenario(
            "ftp-url",
            &[
                "--mcp",
                "ftp://artifacts.example.com/mcp",
                "--html",
                "page.html",
                "--bindings",
                "bindings.json",
                "--title",
                "T",
            ],
        ),
        scenario(
            "missing-html",
            &[
                "--mcp",
                "{MCP}",
                "--html",
                "missing.html",
                "--bindings",
                "bindings.json",
                "--title",
                "T",
            ],
        ),
        scenario(
            "html-is-a-directory",
            &[
                "--mcp",
                "{MCP}",
                "--html",
                ".",
                "--bindings",
                "bindings.json",
                "--title",
                "T",
            ],
        ),
        scenario(
            "connection-refused",
            &[
                "--mcp",
                "{CLOSED}",
                "--html",
                "page.html",
                "--bindings",
                "bindings.json",
                "--title",
                "T",
            ],
        ),
    ];
    for (name, mcp) in [
        (
            "initialize-http-401",
            Mcp {
                http_status: Some(("initialize", 401)),
                ..Mcp::default()
            },
        ),
        (
            "initialize-redirect",
            Mcp {
                http_status: Some(("initialize", 302)),
                ..Mcp::default()
            },
        ),
        (
            "list-rpc-error",
            Mcp {
                rpc_error: Some("list_data_sources"),
                ..Mcp::default()
            },
        ),
        (
            "publish-tool-error",
            Mcp {
                tool_error: Some("publish_artifact"),
                ..Mcp::default()
            },
        ),
        (
            "publish-without-result",
            Mcp {
                empty_result: Some("publish_artifact"),
                ..Mcp::default()
            },
        ),
        (
            "bindings-rejected",
            Mcp {
                tool_error: Some("set_data_bindings"),
                ..Mcp::default()
            },
        ),
        (
            "bindings-http-500",
            Mcp {
                http_status: Some(("set_data_bindings", 500)),
                ..Mcp::default()
            },
        ),
    ] {
        list.push(Scenario {
            mcp,
            ..scenario(name, &base(&[]))
        });
    }
    list
}

/// A scratch directory removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        use std::hash::{BuildHasher, RandomState};
        let suffix = RandomState::new().hash_one((label, std::process::id()));
        let path = std::env::temp_dir().join(format!("xtask-publish-{label}-{suffix:x}"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The wrapper's environment (`artifact_publish.py: minimal_environment`).
fn minimal_command(program: &Path, cwd: &Path, token: Option<&str>) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TMPDIR", "/tmp")
        .current_dir(cwd);
    if let Some(token) = token {
        command.env("ARTIFACT_MCP_API_KEY", token);
    }
    command
}

fn xtask() -> PathBuf {
    std::env::var_os("XTASK_PUBLISH_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_xtask")))
}

fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}/mcp", listener.local_addr().unwrap())
}

/// Run one scenario with `launcher` (the port or Node) and describe what happened.
fn capture(scenario: &Scenario, launcher: &dyn Fn(&Path, Option<&str>) -> Command) -> Value {
    let dir = Scratch::new(scenario.name);
    std::fs::write(dir.0.join("page.html"), html()).unwrap();
    std::fs::write(dir.0.join("bindings.json"), scenario.bindings).unwrap();
    let mcp = scenario.mcp.clone();
    let server = MockServer::start(move |request| mcp.reply(request));
    let closed = closed_port_url();
    let args: Vec<String> = scenario
        .args
        .iter()
        .map(|arg| {
            arg.replace("{MCP}", &format!("{}/mcp", server.base()))
                .replace("{CLOSED}", &closed)
        })
        .collect();
    let output = launcher(&dir.0, scenario.token)
        .args(&args)
        .output()
        .expect("run the publisher");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stdout.contains(TOKEN) && !stderr.contains(TOKEN),
        "{}: the token reached the output",
        scenario.name
    );
    let requests: Vec<Value> = server
        .requests()
        .iter()
        .map(|request| {
            json!({
                "method": request.method,
                "target": request.target,
                "headers": {
                    "accept": request.header("accept"),
                    "authorization": request.header("authorization").map(|value| value.replace(TOKEN, "<TOKEN>")),
                    "content-type": request.header("content-type"),
                },
                "body": request.body_text(),
            })
        })
        .collect();
    json!({
        "exit": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "requests": requests,
    })
}

fn run_port(cwd: &Path, token: Option<&str>) -> Command {
    let mut command = minimal_command(&xtask(), cwd, token);
    command.arg("publish-data-artifact");
    command
}

fn committed_capture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CAPTURE);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap()
}

#[test]
fn every_scenario_matches_the_node_capture() {
    let expected = committed_capture();
    let all = scenarios();
    assert_eq!(
        expected["scenarios"].as_object().unwrap().len(),
        all.len(),
        "the capture and the scenario list disagree"
    );
    for scenario in &all {
        let actual = capture(scenario, &run_port);
        let wanted = &expected["scenarios"][scenario.name];
        assert_eq!(
            &actual["requests"], &wanted["requests"],
            "{}: requests",
            scenario.name
        );
        assert_eq!(
            &actual["stdout"], &wanted["stdout"],
            "{}: stdout",
            scenario.name
        );
        assert_eq!(
            &actual["stderr"], &wanted["stderr"],
            "{}: stderr",
            scenario.name
        );
        assert_eq!(&actual["exit"], &wanted["exit"], "{}: exit", scenario.name);
    }
}

#[test]
fn a_successful_publish_sends_the_documented_sequence() {
    let outcome = capture(&scenarios().remove(0), &run_port);
    let requests = outcome["requests"].as_array().unwrap();
    let methods: Vec<String> = requests
        .iter()
        .map(|request| {
            let body: Value = serde_json::from_str(request["body"].as_str().unwrap()).unwrap();
            format!(
                "{} {}",
                body["method"].as_str().unwrap(),
                body["params"]["name"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        methods,
        [
            "initialize ",
            "tools/call list_data_sources",
            "tools/call publish_artifact",
            "tools/call set_data_bindings"
        ]
    );
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request["method"], "POST");
        assert_eq!(request["target"], "/mcp");
        assert_eq!(request["headers"]["authorization"], "Bearer <TOKEN>");
        assert_eq!(
            request["headers"]["accept"],
            "application/json, text/event-stream"
        );
        let body: Value = serde_json::from_str(request["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["id"], index + 1);
    }
    assert_eq!(outcome["exit"], 0);
    assert_eq!(
        outcome["stdout"],
        "{\n  \"id\": \"a1b2c3d4e5\",\n  \"url\": \"https://artifacts.example.com/a/a1b2c3d4e5\",\n  \"bindings\": [\n    \"2\",\n    \"10\",\n    \"prs\",\n    \"checks\"\n  ]\n}\n"
    );
}

#[test]
fn the_token_never_reaches_output_even_when_it_is_malformed_or_echoed() {
    // `capture` asserts the token is absent from stdout and stderr for every scenario,
    // including servers that echo the authorization header in error bodies.
    for scenario in scenarios() {
        capture(&scenario, &run_port);
    }
    // Node printed a malformed token verbatim ("Headers.append: \"Bearer ...\" is an invalid
    // header value."); the port withholds it.
    let dir = Scratch::new("malformed-token");
    std::fs::write(dir.0.join("page.html"), "<p>x</p>").unwrap();
    std::fs::write(dir.0.join("bindings.json"), BINDINGS).unwrap();
    let server = MockServer::start(|_| Reply::raw(200, "application/json", "{}"));
    let secret = "tok_part_one\nSECRET_PART_TWO";
    let output = run_port(&dir.0, Some(secret))
        .args([
            "--mcp",
            &format!("{}/mcp", server.base()),
            "--html",
            "page.html",
            "--bindings",
            "bindings.json",
            "--title",
            "T",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr,
        "Headers.append: \"Bearer [REDACTED]\" is an invalid header value.\n"
    );
    assert!(!stderr.contains("tok_part_one") && !stderr.contains("SECRET_PART_TWO"));
    assert!(server.requests().is_empty());
}

#[test]
fn flag_errors_help_and_exit_codes() {
    let dir = Scratch::new("flags");
    let run = |args: &[&str]| {
        let output = run_port(&dir.0, Some(TOKEN)).args(args).output().unwrap();
        (
            output.status.code(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    };
    let (code, stdout, stderr) = run(&["--help"]);
    assert_eq!(code, Some(0));
    assert_eq!(stderr, "");
    assert!(stdout.starts_with("Publish or update HTML and attach its live-data bindings.\n\ncargo xtask publish-data-artifact --mcp"));
    assert!(stdout.ends_with("--artifact-id updates an existing artifact.\n"));

    let (code, stdout, stderr) = run(&["--mcp", "http://x/mcp", "--bogus"]);
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
    assert_eq!(
        stderr,
        "TypeError [ERR_PARSE_ARGS_UNKNOWN_OPTION]: Unknown option '--bogus'\n"
    );

    let (code, _, stderr) = run(&["--title", "-dash"]);
    assert_eq!(code, Some(1));
    assert!(stderr.starts_with(
        "TypeError [ERR_PARSE_ARGS_INVALID_OPTION_VALUE]: Option '--title' argument is ambiguous."
    ));
}

/// Regenerate or check the capture against the former Node script.
#[test]
fn node_capture_matches_the_former_script() {
    let Some(dir) = std::env::var_os("XTASK_NODE_PARITY_DIR") else {
        eprintln!("skipped: set XTASK_NODE_PARITY_DIR to compare with publish-data-artifact.mjs");
        return;
    };
    let script = Path::new(&dir).join("publish-data-artifact.mjs");
    assert!(script.is_file(), "{} is missing", script.display());
    let node = which("node");
    let run_node = |cwd: &Path, token: Option<&str>| {
        let mut command = minimal_command(&node, cwd, token);
        command.arg(&script);
        command
    };
    let mut captured = serde_json::Map::new();
    for scenario in scenarios() {
        captured.insert(scenario.name.to_owned(), capture(&scenario, &run_node));
    }
    let document = json!({
        "description": "Requests and output of scripts/publish-data-artifact.mjs (Node) for each scenario in tests/ops_publish_data_artifact.rs, against the same mock MCP server.",
        "node": String::from_utf8(Command::new(&node).arg("--version").output().unwrap().stdout).unwrap().trim(),
        "scenarios": captured,
    });
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CAPTURE);
    if std::env::var_os("XTASK_WRITE_NODE_CAPTURE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&document).unwrap()),
        )
        .unwrap();
        eprintln!("wrote {}", path.display());
    } else {
        assert_eq!(document["scenarios"], committed_capture()["scenarios"]);
    }

    // Argument errors are uncaught exceptions in Node; its stack trace holds the same line.
    let scratch = Scratch::new("node-flags");
    for args in [
        &["--bogus"][..],
        &["x"],
        &["--mcp"],
        &["--help=1"],
        &["--title", "-x"],
        &["-h"],
    ] {
        let node_out = run_node(&scratch.0, Some(TOKEN))
            .args(args)
            .output()
            .unwrap();
        let port_out = run_port(&scratch.0, Some(TOKEN))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(node_out.status.code(), port_out.status.code(), "{args:?}");
        let port_stderr = String::from_utf8(port_out.stderr).unwrap();
        assert!(
            String::from_utf8(node_out.stderr)
                .unwrap()
                .contains(port_stderr.trim_end()),
            "{args:?}: {port_stderr}"
        );
    }
}

fn which(program: &str) -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{program} is not on PATH"))
}
