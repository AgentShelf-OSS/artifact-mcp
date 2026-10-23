//! `cargo xtask tunnel <start|status|validate|rollback>`: manage the opt-in Anthropic MCP Tunnel
//! Compose profile and prove Anthropic can reach Artifact MCP through it.
//!
//! Port of the former `scripts/anthropic-tunnel.mjs`. Docker runs from the repository root, its
//! output is captured and never printed (a failing step reports only its name and exit status),
//! and the validation credentials are read from files and sent only in the Messages API request.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use url::Url;

use super::env_var;
use super::http::Http;
use super::js::{self, Js, object};
use super::node_fs_error;

pub const TUNNEL_PROFILE: &str = "anthropic-tunnel";
pub const TUNNEL_SERVICES: [&str; 2] = ["anthropic-mcp-proxy", "anthropic-mcp-cloudflared"];

pub const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const MCP_CONNECTOR_BETA: &str = "mcp-client-2025-11-20";
const SERVER_NAME: &str = "artifact-mcp-private";
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(45);

const USAGE: &str = "Usage: cargo xtask tunnel <start|status|validate|rollback>

start      Start the opt-in Anthropic MCP Tunnel profile and wait for local health.
status     Check Artifact MCP, proxy, and tunnel-edge health independently.
validate   Run local checks, then call list_artifacts from Anthropic's Messages API.
rollback   Stop the tunnel profile and preserve the default Artifact MCP deployment.

validate requires:
  ANTHROPIC_TUNNEL_MCP_URL
  ANTHROPIC_TUNNEL_TEST_MODEL
  ANTHROPIC_API_KEY_FILE
  ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE";

/// One `docker` invocation of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub args: Vec<String>,
    /// The step fails when it exits 0 but prints nothing.
    pub require_output: bool,
}

/// What one `docker` run produced.
#[derive(Clone, Debug, Default)]
pub struct SpawnResult {
    /// Exit code; `None` when a signal ended the process.
    pub status: Option<i32>,
    pub stdout: String,
    /// Docker could not be started at all.
    pub error: bool,
}

/// Runs `docker <args>`; injectable for tests.
pub type Spawn<'a> = dyn FnMut(&[String]) -> SpawnResult + 'a;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn compose(args: &[&str]) -> Vec<String> {
    let mut all = strings(&["compose", "--profile", TUNNEL_PROFILE]);
    all.extend(strings(args));
    all
}

fn step(name: &'static str, args: Vec<String>) -> Step {
    Step {
        name,
        args,
        require_output: false,
    }
}

/// The Docker steps of `start` or `rollback`.
pub fn operation_plan(action: &str) -> Result<Vec<Step>, String> {
    match action {
        "start" => {
            let mut args = vec!["up", "-d", "--wait", "--wait-timeout", "60", "artifact-mcp"];
            args.extend(TUNNEL_SERVICES);
            Ok(vec![step(
                "start Artifact MCP and the private tunnel profile",
                compose(&args),
            )])
        }
        "rollback" => Ok(vec![
            step(
                "restore the default Artifact MCP service",
                strings(&[
                    "compose",
                    "up",
                    "-d",
                    "--wait",
                    "--wait-timeout",
                    "60",
                    "artifact-mcp",
                ]),
            ),
            step(
                "stop the private tunnel profile",
                compose(&["stop", "anthropic-mcp-cloudflared", "anthropic-mcp-proxy"]),
            ),
        ]),
        other => Err(format!("Unsupported tunnel action: {other}")),
    }
}

/// The three independent local health checks.
pub fn local_health_plan() -> Vec<Step> {
    vec![
        step(
            "Artifact MCP application health",
            strings(&[
                "compose",
                "exec",
                "-T",
                "artifact-mcp",
                "/artifact-mcp",
                "healthcheck",
            ]),
        ),
        Step {
            name: "Anthropic MCP proxy process",
            args: compose(&["ps", "--status", "running", "-q", "anthropic-mcp-proxy"]),
            require_output: true,
        },
        step(
            "outbound tunnel edge connectivity",
            compose(&[
                "exec",
                "-T",
                "anthropic-mcp-cloudflared",
                "cloudflared",
                "tunnel",
                "--metrics",
                "127.0.0.1:2000",
                "ready",
            ]),
        ),
    ]
}

/// Run one step; the error names the step and its status, never Docker's output.
pub fn run_docker_step(step: &Step, spawn: &mut Spawn<'_>) -> Result<(), String> {
    let result = spawn(&step.args);
    let output = js::js_trim(&result.stdout);
    if result.error || result.status != Some(0) || (step.require_output && output.is_empty()) {
        let status = if result.error {
            "could not run Docker".to_owned()
        } else {
            format!(
                "exit {}",
                result
                    .status
                    .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
            )
        };
        return Err(format!("{} failed ({status})", step.name));
    }
    Ok(())
}

pub fn run_operation(
    action: &str,
    spawn: &mut Spawn<'_>,
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    for step in operation_plan(action)? {
        run_docker_step(&step, spawn)?;
        log(&format!("[ok] {}", step.name));
    }
    Ok(())
}

pub fn check_local_health(spawn: &mut Spawn<'_>, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    for step in local_health_plan() {
        run_docker_step(&step, spawn)?;
        log(&format!("[ok] {}", step.name));
    }
    Ok(())
}

/// Docker from the repository root, with stdin closed and output captured.
fn docker_spawn(args: &[String]) -> SpawnResult {
    match Command::new("docker")
        .args(args)
        .current_dir(crate::repo_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => SpawnResult {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            error: false,
        },
        Err(_) => SpawnResult {
            error: true,
            ..SpawnResult::default()
        },
    }
}

fn required_env(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    let value = js::js_trim(env(name).as_deref().unwrap_or_default()).to_owned();
    if value.is_empty() {
        return Err(format!("{name} is required"));
    }
    Ok(value)
}

/// Accept only an HTTPS `*.tunnel.anthropic.com` URL for Artifact MCP's `/mcp` path.
pub fn validate_tunnel_url(value: &str) -> Result<String, String> {
    let url = Url::parse(value)
        .map_err(|_| "ANTHROPIC_TUNNEL_MCP_URL must be a valid HTTPS URL".to_owned())?;
    if url.scheme() != "https"
        || !url
            .host_str()
            .unwrap_or_default()
            .ends_with(".tunnel.anthropic.com")
        || !url.username().is_empty()
        || url.password().is_some_and(|password| !password.is_empty())
        || url.query().is_some_and(|query| !query.is_empty())
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
    {
        return Err(
            "ANTHROPIC_TUNNEL_MCP_URL must be an HTTPS *.tunnel.anthropic.com URL without credentials, query, or fragment"
                .to_owned(),
        );
    }
    if url.path().trim_end_matches('/') != "/mcp" {
        return Err("ANTHROPIC_TUNNEL_MCP_URL must use Artifact MCP's /mcp path".to_owned());
    }
    Ok(url.to_string())
}

/// Read a one-line secret file.
pub fn read_secret(path: &str) -> Result<String, String> {
    let raw = std::fs::read(path).map_err(|error| node_fs_error(&error, "open", path))?;
    let value = js::js_trim(&String::from_utf8_lossy(&raw)).to_owned();
    if value.is_empty() || value.encode_utf16().count() > 16_384 || value.contains(['\r', '\n']) {
        return Err(format!(
            "Secret file {path} must contain one non-empty line"
        ));
    }
    Ok(value)
}

/// The Messages API request: Claude may call only the read-only `list_artifacts` tool.
pub fn build_remote_validation_request(tunnel_url: &str, upstream_token: &str, model: &str) -> Js {
    object([
        ("model", Js::from(model)),
        ("max_tokens", Js::from(256_u64)),
        (
            "messages",
            Js::Array(vec![object([
                ("role", Js::from("user")),
                (
                    "content",
                    Js::from(
                        "Call list_artifacts exactly once with an empty input, then say validation complete.",
                    ),
                ),
            ])]),
        ),
        (
            "mcp_servers",
            Js::Array(vec![object([
                ("type", Js::from("url")),
                ("url", Js::from(tunnel_url)),
                ("name", Js::from(SERVER_NAME)),
                ("authorization_token", Js::from(upstream_token)),
            ])]),
        ),
        (
            "tools",
            Js::Array(vec![object([
                ("type", Js::from("mcp_toolset")),
                ("mcp_server_name", Js::from(SERVER_NAME)),
                ("default_config", object([("enabled", Js::Bool(false))])),
                (
                    "configs",
                    object([(
                        "list_artifacts",
                        object([
                            ("enabled", Js::Bool(true)),
                            ("defer_loading", Js::Bool(false)),
                        ]),
                    )]),
                ),
            ])]),
        ),
    ])
}

/// Ask Anthropic's Messages API to call `list_artifacts` through the tunnel.
pub fn validate_remote_registration(
    env: &dyn Fn(&str) -> Option<String>,
    messages_url: &str,
    secret_reader: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let tunnel_url = validate_tunnel_url(&required_env(env, "ANTHROPIC_TUNNEL_MCP_URL")?)?;
    let model = required_env(env, "ANTHROPIC_TUNNEL_TEST_MODEL")?;
    let api_key = secret_reader(&required_env(env, "ANTHROPIC_API_KEY_FILE")?)?;
    let upstream_token =
        secret_reader(&required_env(env, "ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE")?)?;

    const UNREACHABLE: &str =
        "Anthropic-side MCP tunnel validation could not reach the Messages API";
    let http = Http::new(Some(VALIDATION_TIMEOUT), true).map_err(|_| UNREACHABLE.to_owned())?;
    let body = js::stringify(&build_remote_validation_request(
        &tunnel_url,
        &upstream_token,
        &model,
    ));
    let response = http
        .send(
            "POST",
            messages_url,
            &[
                ("content-type", "application/json"),
                ("x-api-key", &api_key),
                ("anthropic-version", "2023-06-01"),
                ("anthropic-beta", MCP_CONNECTOR_BETA),
            ],
            Some(body.into_bytes()),
        )
        .map_err(|_| UNREACHABLE.to_owned())?;

    let payload = js::parse(&response.text()).map_err(|_| {
        format!(
            "Anthropic-side MCP tunnel validation returned non-JSON HTTP {}",
            response.status
        )
    })?;
    if !response.ok() {
        let error_type = js::get_opt(payload.property("error"), "type");
        let error_type = if js::truthy(error_type) {
            js::to_js_string(error_type)
        } else {
            "unknown_error".to_owned()
        };
        return Err(format!(
            "Anthropic-side MCP tunnel validation failed with HTTP {} ({error_type})",
            response.status
        ));
    }

    let blocks: &[Js] = match payload.property("content") {
        Some(Js::Array(blocks)) => blocks,
        _ => &[],
    };
    let tool_use = blocks.iter().find(|block| {
        js::is_str(block.property("type"), "mcp_tool_use")
            && js::is_str(block.property("server_name"), SERVER_NAME)
            && js::is_str(block.property("name"), "list_artifacts")
    });
    let tool_use_id = js::get_opt(tool_use, "id");
    let tool_result = blocks.iter().find(|block| {
        js::is_str(block.property("type"), "mcp_tool_result")
            && js::strict_equals(block.property("tool_use_id"), tool_use_id)
            && block.property("is_error") != Some(&Js::Bool(true))
    });
    if tool_use.is_none() || tool_result.is_none() {
        return Err(
            "Anthropic reached the tunnel but did not complete the read-only list_artifacts probe"
                .to_owned(),
        );
    }
    Ok(())
}

/// Entry point for `cargo xtask tunnel`.
pub fn main(args: &[String]) -> bool {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run(
        args,
        &mut docker_spawn,
        &env_var,
        ANTHROPIC_MESSAGES_URL,
        &read_secret,
        &mut stdout.lock(),
        &mut stderr.lock(),
    )
}

/// Run the command with injectable Docker, environment, Messages API URL and output streams.
pub(crate) fn run(
    args: &[String],
    spawn: &mut Spawn<'_>,
    env: &dyn Fn(&str) -> Option<String>,
    messages_url: &str,
    secret_reader: &dyn Fn(&str) -> Result<String, String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    let action = args.first().map(String::as_str).unwrap_or_default();
    if action.is_empty() || action == "--help" || action == "-h" {
        let _ = writeln!(out, "{USAGE}");
        return true;
    }
    if args.len() > 1 || !["start", "status", "validate", "rollback"].contains(&action) {
        let _ = writeln!(err, "{USAGE}");
        return false;
    }
    let mut log = |line: &str| {
        let _ = writeln!(out, "{line}");
    };
    let result = (|| -> Result<(), String> {
        match action {
            "start" => {
                run_operation("start", spawn, &mut log)?;
                check_local_health(spawn, &mut log)?;
                log(
                    "Tunnel profile is locally healthy. Run `cargo xtask tunnel validate` before cutover.",
                );
            }
            "rollback" => {
                run_operation("rollback", spawn, &mut log)?;
                let app_health = local_health_plan().swap_remove(0);
                run_docker_step(&app_health, spawn)?;
                log(&format!("[ok] {}", app_health.name));
                log(
                    "Default deployment restored. Re-enable the former public /mcp edge policy if it was removed after cutover.",
                );
            }
            _ => {
                check_local_health(spawn, &mut log)?;
                if action == "validate" {
                    validate_remote_registration(env, messages_url, secret_reader)?;
                    log(
                        "[ok] Anthropic-side registration, routing, authentication, and read-only tool call",
                    );
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            false
        }
    }
}

#[cfg(test)]
#[path = "tunnel_tests.rs"]
mod tests;
