//! `cf-access-setup` against a recording mock Cloudflare API.
//!
//! Each case asserts the exact request sequence (method, target, raw body bytes), stdout,
//! stderr and exit status, and that preview never sends a mutating request, that no request
//! touches an Access policy, and that the API token appears only in the `authorization` header.
//!
//! Set `XTASK_NODE_PARITY_DIR` to a directory holding the former `cf-access-setup.mjs` to also
//! run every case through Node (its `fetch` rewritten to the mock) and require identical
//! requests and output.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use super::run;
use crate::ops::mock_http::{MockServer, Recorded, Reply};

const TOKEN: &str = "cf-token-must-stay-in-the-authorization-header";
const ACCOUNT_PATH: &str = "/client/v4/accounts/acct%201%2Fx";
const ZONE_PATH: &str = "/client/v4/zones/zone-1";
const DIGEST: &str = "5b5ef48f425c"; // sha256("artifacts.example.com")[..12]

type Env = Vec<(&'static str, &'static str)>;

/// A scripted Cloudflare account.
#[derive(Clone)]
struct Fake {
    organization: Value,
    app_pages: Vec<Vec<Value>>,
    rulesets: Vec<Value>,
    ruleset: Value,
    /// (method, target prefix, status, raw body) answered before any route.
    failures: Vec<(&'static str, String, u16, String)>,
}

impl Fake {
    fn fresh() -> Self {
        Self {
            organization: json!({
                "auth_domain": "team.cloudflareaccess.com",
                "name": "Team",
                "is_ui_read_only": false,
                "session_duration": "24h",
                "created_at": "2026-01-01T00:00:00Z"
            }),
            app_pages: vec![vec![]],
            rulesets: vec![],
            ruleset: Value::Null,
            failures: vec![],
        }
    }

    fn reply(&self, request: &Recorded) -> Reply {
        for (method, prefix, status, body) in &self.failures {
            if request.method == *method && request.target.starts_with(prefix.as_str()) {
                return Reply::raw(*status, "application/json", body.clone());
            }
        }
        let ok = |result: Value| {
            Reply::json(
                200,
                &json!({"success": true, "errors": [], "messages": [], "result": result}),
            )
        };
        let path = request.path();
        let method = request.method.as_str();
        let apps = format!("{ACCOUNT_PATH}/access/apps");
        let organizations = format!("{ACCOUNT_PATH}/access/organizations");
        let rulesets = format!("{ZONE_PATH}/rulesets");
        if path == organizations && method == "GET" {
            return ok(self.organization.clone());
        }
        if path == organizations && method == "PUT" {
            return ok(request.json());
        }
        if path == apps && method == "GET" {
            let page: usize = request
                .target
                .rsplit("page=")
                .next()
                .and_then(|page| page.parse().ok())
                .unwrap_or(1);
            return Reply::json(
                200,
                &json!({
                    "success": true,
                    "errors": [],
                    "messages": [],
                    "result": self.app_pages.get(page - 1).cloned().unwrap_or_default(),
                    "result_info": {"page": page, "per_page": 100, "total_pages": self.app_pages.len()}
                }),
            );
        }
        if path == apps && method == "POST" {
            return ok(json!({"id": "app-created", "aud": "aud-created"}));
        }
        if let Some(id) = path.strip_prefix(&format!("{apps}/"))
            && method == "PUT"
        {
            return ok(json!({"id": id, "aud": "aud-updated"}));
        }
        if path == rulesets && method == "GET" {
            return ok(Value::Array(self.rulesets.clone()));
        }
        if path == rulesets && method == "POST" {
            return ok(json!({"id": "rs-created"}));
        }
        if path.starts_with(&format!("{rulesets}/")) {
            return match method {
                "GET" => ok(self.ruleset.clone()),
                "PATCH" | "POST" => ok(json!({"id": "rs-updated"})),
                _ => not_found(),
            };
        }
        not_found()
    }
}

fn not_found() -> Reply {
    Reply::json(
        404,
        &json!({"success": false, "errors": [{"code": 7003, "message": "No route for that URI"}], "messages": []}),
    )
}

fn base_env() -> Env {
    vec![
        ("CF_API_TOKEN", TOKEN),
        ("CF_ACCOUNT_ID", "acct 1/x"),
        ("PUBLIC_BASE_URL", "https://Artifacts.Example.com/"),
        ("CF_ACCESS_IDP_ID", "idp-1"),
        ("CF_ZONE_ID", "zone-1"),
    ]
}

fn with(mut env: Env, extra: &[(&'static str, &'static str)]) -> Env {
    for (name, value) in extra {
        env.retain(|(existing, _)| existing != name);
        env.push((name, value));
    }
    env
}

fn without(mut env: Env, name: &str) -> Env {
    env.retain(|(existing, _)| *existing != name);
    env
}

struct Outcome {
    ok: bool,
    stdout: String,
    stderr: String,
    requests: Vec<Recorded>,
}

fn invoke(fake: &Fake, args: &[&str], env: &[(&'static str, &'static str)]) -> Outcome {
    let scripted = fake.clone();
    let server = MockServer::start(move |request| scripted.reply(request));
    let api_base = format!("{}/client/v4", server.base());
    invoke_at(&server, &api_base, args, env)
}

fn invoke_at(
    server: &MockServer,
    api_base: &str,
    args: &[&str],
    env: &[(&'static str, &'static str)],
) -> Outcome {
    let map: HashMap<&str, &str> = env.iter().copied().collect();
    let lookup = |name: &str| map.get(name).map(|value| (*value).to_owned());
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let ok = run(&args, &lookup, api_base, &mut out, &mut err);
    let outcome = Outcome {
        ok,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
        requests: server.requests(),
    };
    assert_safe(&outcome, args.iter().any(|arg| arg == "--apply"));
    outcome
}

/// Least privilege: the token only in `authorization`, JSON bodies typed, never an Access
/// policy, and a preview that reads only.
fn assert_safe(outcome: &Outcome, apply: bool) {
    assert!(!outcome.stdout.contains(TOKEN) && !outcome.stderr.contains(TOKEN));
    for request in &outcome.requests {
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert!(!request.target.contains(TOKEN));
        assert!(
            !request.target.to_ascii_lowercase().contains("polic"),
            "{}",
            request.target
        );
        let expected_type = (!request.body.is_empty()).then_some("application/json");
        assert_eq!(
            request.header("content-type"),
            expected_type,
            "{}",
            request.target
        );
        if !apply {
            assert_eq!(request.method, "GET", "preview mutated {}", request.target);
        }
    }
}

/// `(method, target, raw body)` per request.
fn calls(outcome: &Outcome) -> Vec<(String, String, String)> {
    outcome
        .requests
        .iter()
        .map(|request| {
            (
                request.method.clone(),
                request.target.clone(),
                request.body_text(),
            )
        })
        .collect()
}

fn get(target: &str) -> (String, String, String) {
    ("GET".to_owned(), target.to_owned(), String::new())
}

fn send(method: &str, target: &str, body: &str) -> (String, String, String) {
    (method.to_owned(), target.to_owned(), body.to_owned())
}

fn closing(app_suffix: &str) -> String {
    format!(
        "\nRemaining operator-owned security actions (this script never changes policies):
  1. Create or retain an Access application for artifacts.example.com/mcp with Bypass -> Everyone.
  2. Create or retain an Access application for artifacts.example.com/s/* with Bypass -> Everyone.
  3. Create or retain the intended-viewer Allow policy on the catch-all artifacts.example.com app{app_suffix}.
  4. Set CF_ACCESS_AUD, CF_ACCESS_TEAM_DOMAIN, and REQUIRE_ACCESS_JWT=1 in the runtime environment.
  5. Fully restart artifact-mcp; editing an env file does not update module-initialized identity settings.\n"
    )
}

fn desired_rule() -> String {
    format!(
        r#"{{"ref":"artifact_mcp_email_off_{DIGEST}","expression":"http.host eq \"artifacts.example.com\"","description":"artifact-mcp: disable Email Obfuscation for artifacts.example.com","action":"set_config","action_parameters":{{"email_obfuscation":false}},"enabled":true}}"#
    )
}

const BRANDING: [(&str, &str); 2] = [
    ("CF_ACCESS_LOGIN_HEADER", "Artifacts"),
    ("CF_ACCESS_LOGIN_BACKGROUND_COLOR", "#142235"),
];

const FRESH_DESIGN: &str = r##"Account-wide login design (affects every Access application):
{
  "current": {},
  "desired": {
    "header_text": "Artifacts",
    "background_color": "#142235"
  }
}
"##;

fn first_run_case() -> (Fake, Env) {
    (Fake::fresh(), with(base_env(), &BRANDING))
}

#[test]
fn first_run_preview_reads_only_and_reports_every_planned_change() {
    let (fake, env) = first_run_case();
    let outcome = invoke(&fake, &[], &env);
    assert!(outcome.ok, "{}", outcome.stderr);
    assert_eq!(
        calls(&outcome),
        vec![
            get(&format!("{ACCOUNT_PATH}/access/organizations")),
            get(&format!("{ACCOUNT_PATH}/access/apps?per_page=100&page=1")),
            get(&format!("{ZONE_PATH}/rulesets?per_page=100")),
        ]
    );
    assert_eq!(
        outcome.stdout,
        format!(
            "artifact-mcp Cloudflare Access setup (dry-run)
Hostname: artifacts.example.com
[dry-run] Access application: would create catch-all artifacts.example.com
{FRESH_DESIGN}[dry-run] Account-wide login design: would update reusable Access Custom Page branding
[dry-run] Configuration Rule: would create for artifacts.example.com

Runtime values:
CF_ACCESS_AUD=<created by a subsequent --apply run>
CF_ACCESS_TEAM_DOMAIN=team.cloudflareaccess.com
{}",
            closing("")
        )
    );
    assert_eq!(outcome.stderr, "");
}

#[test]
fn first_run_apply_creates_the_app_branding_and_rule_in_order() {
    let (fake, env) = first_run_case();
    let outcome = invoke(&fake, &["--apply"], &env);
    assert!(outcome.ok, "{}", outcome.stderr);
    assert_eq!(
        calls(&outcome),
        vec![
            get(&format!("{ACCOUNT_PATH}/access/organizations")),
            get(&format!("{ACCOUNT_PATH}/access/apps?per_page=100&page=1")),
            send(
                "POST",
                &format!("{ACCOUNT_PATH}/access/apps"),
                r#"{"name":"artifact-mcp (artifacts.example.com)","domain":"artifacts.example.com","type":"self_hosted","destinations":[{"type":"public","uri":"artifacts.example.com"}],"allowed_idps":["idp-1"],"auto_redirect_to_identity":true,"app_launcher_visible":false,"session_duration":"24h"}"#,
            ),
            send(
                "PUT",
                &format!("{ACCOUNT_PATH}/access/organizations"),
                r##"{"auth_domain":"team.cloudflareaccess.com","is_ui_read_only":false,"name":"Team","session_duration":"24h","login_design":{"header_text":"Artifacts","background_color":"#142235"}}"##,
            ),
            get(&format!("{ZONE_PATH}/rulesets?per_page=100")),
            send(
                "POST",
                &format!("{ZONE_PATH}/rulesets"),
                &format!(
                    r#"{{"name":"artifact-mcp configuration rules","description":"Host-specific response transformation safeguards for artifact-mcp","kind":"zone","phase":"http_config_settings","rules":[{}]}}"#,
                    desired_rule()
                ),
            ),
        ]
    );
    assert_eq!(
        outcome.stdout,
        format!(
            "artifact-mcp Cloudflare Access setup (apply)
Hostname: artifacts.example.com
Access application: created artifacts.example.com
{FRESH_DESIGN}Account-wide login design: updated
Configuration Rule: created with the phase ruleset

Runtime values:
CF_ACCESS_AUD=aud-created
CF_ACCESS_TEAM_DOMAIN=team.cloudflareaccess.com
{}",
            closing(" (app-created)")
        )
    );
    assert_eq!(outcome.stderr, "");
}

fn drifted_case() -> (Fake, Env) {
    let mut fake = Fake::fresh();
    fake.organization = json!({
        "auth_domain": "team.cloudflareaccess.com",
        "login_design": {"background_color": "#142235", "header_text": "Artifacts", "logo_path": "https://cdn.example.com/logo.png"}
    });
    fake.app_pages = vec![
        vec![
            json!({"id": "other", "type": "self_hosted", "domain": "other.example.com"}),
            json!({"id": "bookmark", "type": "bookmark", "domain": "artifacts.example.com"}),
        ],
        vec![json!({
            "id": "app-1",
            "type": "self_hosted",
            "name": "Artifacts",
            "domain": "",
            "destinations": [{"type": "public", "uri": "https://ARTIFACTS.example.com/"}],
            "allowed_idps": ["idp-1", "idp-2"],
            "auto_redirect_to_identity": false,
            "session_duration": "12h",
            "app_launcher_visible": true,
            "tags": ["a"],
            "cors_headers": null,
            "aud": "aud-1",
            "created_at": "2026-01-01T00:00:00Z"
        })],
    ];
    fake.rulesets = vec![
        json!({"id": "rs-managed", "kind": "managed", "phase": "http_config_settings"}),
        json!({"id": "rs 1", "kind": "zone", "phase": "http_config_settings"}),
    ];
    fake.ruleset = json!({
        "id": "rs 1",
        "rules": [{
            "id": "rule-a",
            "ref": "hand-made",
            "description": "artifact-mcp: disable Email Obfuscation for artifacts.example.com",
            "action": "set_config",
            "expression": "http.host eq \"old.example.com\"",
            "action_parameters": {"email_obfuscation": false}
        }]
    });
    (fake, with(base_env(), &BRANDING))
}

const DRIFTED_DESIGN: &str = r##"Account-wide login design (affects every Access application):
{
  "current": {
    "background_color": "#142235",
    "header_text": "Artifacts",
    "logo_path": "https://cdn.example.com/logo.png"
  },
  "desired": {
    "background_color": "#142235",
    "header_text": "Artifacts",
    "logo_path": "https://cdn.example.com/logo.png"
  }
}
Account-wide login design: no-op
"##;

#[test]
fn drifted_preview_pages_through_apps_and_inspects_the_ruleset() {
    let (fake, env) = drifted_case();
    let outcome = invoke(&fake, &[], &env);
    assert!(outcome.ok, "{}", outcome.stderr);
    assert_eq!(
        calls(&outcome),
        vec![
            get(&format!("{ACCOUNT_PATH}/access/organizations")),
            get(&format!("{ACCOUNT_PATH}/access/apps?per_page=100&page=1")),
            get(&format!("{ACCOUNT_PATH}/access/apps?per_page=100&page=2")),
            get(&format!("{ZONE_PATH}/rulesets?per_page=100")),
            get(&format!("{ZONE_PATH}/rulesets/rs%201")),
        ]
    );
    assert_eq!(
        outcome.stdout,
        format!(
            "artifact-mcp Cloudflare Access setup (dry-run)
Hostname: artifacts.example.com
[dry-run] Access application: would enable direct single-IdP login on app-1
{DRIFTED_DESIGN}[dry-run] Configuration Rule: would update for artifacts.example.com

Runtime values:
CF_ACCESS_AUD=aud-1
CF_ACCESS_TEAM_DOMAIN=team.cloudflareaccess.com
{}",
            closing(" (app-1)")
        )
    );
}

#[test]
fn drifted_apply_updates_the_app_and_patches_the_rule() {
    let (fake, env) = drifted_case();
    let outcome = invoke(&fake, &["--apply"], &env);
    assert!(outcome.ok, "{}", outcome.stderr);
    assert_eq!(
        calls(&outcome),
        vec![
            get(&format!("{ACCOUNT_PATH}/access/organizations")),
            get(&format!("{ACCOUNT_PATH}/access/apps?per_page=100&page=1")),
            get(&format!("{ACCOUNT_PATH}/access/apps?per_page=100&page=2")),
            send(
                "PUT",
                &format!("{ACCOUNT_PATH}/access/apps/app-1"),
                r#"{"name":"Artifacts","domain":"artifacts.example.com","type":"self_hosted","allowed_idps":["idp-1"],"app_launcher_visible":true,"auto_redirect_to_identity":true,"cors_headers":null,"destinations":[{"type":"public","uri":"https://ARTIFACTS.example.com/"}],"session_duration":"24h","tags":["a"]}"#,
            ),
            get(&format!("{ZONE_PATH}/rulesets?per_page=100")),
            get(&format!("{ZONE_PATH}/rulesets/rs%201")),
            send(
                "PATCH",
                &format!("{ZONE_PATH}/rulesets/rs%201/rules/rule-a"),
                &desired_rule(),
            ),
        ]
    );
    assert_eq!(
        outcome.stdout,
        format!(
            "artifact-mcp Cloudflare Access setup (apply)
Hostname: artifacts.example.com
Access application: updated app-1
{DRIFTED_DESIGN}Configuration Rule: updated

Runtime values:
CF_ACCESS_AUD=aud-updated
CF_ACCESS_TEAM_DOMAIN=team.cloudflareaccess.com
{}",
            closing(" (app-1)")
        )
    );
}

fn converged_case() -> (Fake, Env) {
    let mut fake = Fake::fresh();
    fake.app_pages = vec![vec![json!({
        "id": "app-1",
        "type": "self_hosted",
        "domain": "artifacts.example.com",
        "allowed_idps": ["idp-1"],
        "auto_redirect_to_identity": true,
        "session_duration": "1h30m",
        "aud": "aud-1"
    })]];
    fake.rulesets = vec![json!({"id": "rs-1", "kind": "zone", "phase": "http_config_settings"})];
    fake.ruleset = json!({
        "id": "rs-1",
        "rules": [
            {"id": "rule-x", "ref": "unrelated", "description": "other", "action": "set_config"},
            {
                "id": "rule-b",
                "ref": format!("artifact_mcp_email_off_{DIGEST}"),
                "description": "renamed by an operator",
                "action": "set_config",
                "expression": "http.host eq \"artifacts.example.com\"",
                "action_parameters": {"email_obfuscation": false}
            }
        ]
    });
    (
        fake,
        with(base_env(), &[("CF_ACCESS_SESSION_DURATION", "1h30m")]),
    )
}

#[test]
fn converged_account_is_a_no_op_even_with_apply() {
    let (fake, env) = converged_case();
    let outcome = invoke(&fake, &["--apply", "--apply"], &env);
    assert!(outcome.ok, "{}", outcome.stderr);
    assert!(
        outcome
            .requests
            .iter()
            .all(|request| request.method == "GET")
    );
    assert_eq!(
        calls(&outcome),
        vec![
            get(&format!("{ACCOUNT_PATH}/access/organizations")),
            get(&format!("{ACCOUNT_PATH}/access/apps?per_page=100&page=1")),
            get(&format!("{ZONE_PATH}/rulesets?per_page=100")),
            get(&format!("{ZONE_PATH}/rulesets/rs-1")),
        ]
    );
    assert_eq!(
        outcome.stdout,
        format!(
            "artifact-mcp Cloudflare Access setup (apply)
Hostname: artifacts.example.com
Access application: no-op (app-1)
Account-wide login design: no inputs supplied; skipped
Configuration Rule: no-op

Runtime values:
CF_ACCESS_AUD=aud-1
CF_ACCESS_TEAM_DOMAIN=team.cloudflareaccess.com
{}",
            closing(" (app-1)")
        )
    );
}

#[test]
fn missing_zone_skips_the_rule_with_a_warning() {
    let (fake, env) = converged_case();
    let outcome = invoke(&fake, &[], &without(env, "CF_ZONE_ID"));
    assert!(outcome.ok);
    assert_eq!(outcome.requests.len(), 2);
    assert_eq!(
        outcome.stderr,
        "Configuration Rule: skipped (CF_ZONE_ID is unset; origin no-transform remains active)\n"
    );
}

#[test]
fn ruleset_failures_warn_and_continue() {
    let (mut fake, env) = drifted_case();
    fake.failures.push((
        "GET",
        format!("{ZONE_PATH}/rulesets?"),
        500,
        r#"{"success":false,"errors":[{"code":1000,"message":"boom"},{"message":"retry later"}],"messages":[{"code":0,"message":""}]}"#.to_owned(),
    ));
    let outcome = invoke(&fake, &["--apply"], &env);
    assert!(outcome.ok);
    assert_eq!(
        outcome.stderr,
        "Configuration Rule: skipped (Cloudflare API GET /zones/zone-1/rulesets?per_page=100 failed (500): 1000: boom; retry later; origin no-transform remains active)\n"
    );

    let (mut fake, env) = drifted_case();
    fake.rulesets
        .push(json!({"id": "rs-2", "kind": "zone", "phase": "http_config_settings"}));
    let outcome = invoke(&fake, &[], &env);
    assert!(outcome.ok);
    assert_eq!(
        outcome.stderr,
        "Configuration Rule: skipped (multiple zone entry-point rulesets exist for http_config_settings; origin no-transform remains active)\n"
    );
}

fn forbidden() -> Fake {
    let mut fake = Fake::fresh();
    fake.failures.push((
        "GET",
        format!("{ACCOUNT_PATH}/access/organizations"),
        403,
        r#"{"success":false,"errors":[{"code":10000,"message":"Authentication error"}],"messages":[]}"#.to_owned(),
    ));
    fake
}

#[test]
fn api_failures_stop_setup_with_exit_1() {
    let outcome = invoke(&forbidden(), &["--apply"], &base_env());
    assert!(!outcome.ok);
    assert_eq!(
        outcome.stdout,
        "artifact-mcp Cloudflare Access setup (apply)\nHostname: artifacts.example.com\n"
    );
    assert_eq!(
        outcome.stderr,
        "Setup failed: Cloudflare API GET /accounts/acct%201%2Fx/access/organizations failed (403): 10000: Authentication error\n"
    );

    let mut fake = Fake::fresh();
    fake.failures.push((
        "GET",
        format!("{ACCOUNT_PATH}/access/apps"),
        200,
        r#"{"success":false,"errors":[],"messages":[]}"#.to_owned(),
    ));
    let outcome = invoke(&fake, &[], &base_env());
    assert_eq!(
        outcome.stderr,
        "Setup failed: Cloudflare API GET /accounts/acct%201%2Fx/access/apps?per_page=100&page=1 failed (200)\n"
    );

    let mut fake = Fake::fresh();
    fake.failures.push((
        "GET",
        format!("{ACCOUNT_PATH}/access/apps"),
        502,
        "<html>bad gateway</html>".to_owned(),
    ));
    let outcome = invoke(&fake, &[], &base_env());
    assert_eq!(
        outcome.stderr,
        "Setup failed: Cloudflare API GET /accounts/acct%201%2Fx/access/apps?per_page=100&page=1 returned non-JSON status 502\n"
    );

    let mut fake = Fake::fresh();
    fake.app_pages = vec![vec![
        json!({"id": "a", "type": "self_hosted", "domain": "artifacts.example.com"}),
        json!({"id": "b", "type": "self_hosted", "destinations": [{"type": "public", "uri": "artifacts.example.com/"}]}),
    ]];
    let outcome = invoke(&fake, &["--apply"], &base_env());
    assert!(!outcome.ok);
    assert!(
        outcome
            .requests
            .iter()
            .all(|request| request.method == "GET")
    );
    assert_eq!(
        outcome.stderr,
        "Setup failed: More than one self-hosted Access application matches artifacts.example.com; resolve the duplicate before continuing\n"
    );

    let mut fake = Fake::fresh();
    fake.organization = Value::Null;
    let outcome = invoke(&fake, &[], &base_env());
    assert_eq!(
        outcome.stderr,
        "Setup failed: Cloudflare did not return a Zero Trust organization for this account\n"
    );
}

#[test]
fn unreachable_api_reports_fetch_failed() {
    let server = MockServer::start(|_| Reply::raw(200, "text/plain", ""));
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/client/v4", listener.local_addr().unwrap())
    };
    let outcome = invoke_at(&server, &closed, &[], &base_env());
    assert!(!outcome.ok);
    assert_eq!(
        outcome.stderr,
        "Setup failed: Cloudflare API request failed: fetch failed\n"
    );
}

fn invalid_configurations() -> Vec<(Env, &'static str)> {
    vec![
        (
            without(base_env(), "CF_API_TOKEN"),
            "CF_API_TOKEN is required",
        ),
        (
            with(base_env(), &[("CF_ACCOUNT_ID", "  ")]),
            "CF_ACCOUNT_ID is required",
        ),
        (
            without(base_env(), "CF_ACCESS_IDP_ID"),
            "CF_ACCESS_IDP_ID is required",
        ),
        (
            with(base_env(), &[("PUBLIC_BASE_URL", "not a url")]),
            "PUBLIC_BASE_URL must be a valid HTTPS URL",
        ),
        (
            with(
                base_env(),
                &[("PUBLIC_BASE_URL", "http://artifacts.example.com")],
            ),
            "PUBLIC_BASE_URL must be a public HTTPS URL without embedded credentials",
        ),
        (
            with(
                base_env(),
                &[("PUBLIC_BASE_URL", "https://user:pw@artifacts.example.com")],
            ),
            "PUBLIC_BASE_URL must be a public HTTPS URL without embedded credentials",
        ),
        (
            with(
                base_env(),
                &[("PUBLIC_BASE_URL", "https://artifacts.example.com/app")],
            ),
            "PUBLIC_BASE_URL must identify a hostname, without a path, query, or fragment",
        ),
        (
            with(
                base_env(),
                &[("PUBLIC_BASE_URL", "https://artifacts.example.com/?a=1")],
            ),
            "PUBLIC_BASE_URL must identify a hostname, without a path, query, or fragment",
        ),
        (
            with(base_env(), &[("CF_ACCESS_SESSION_DURATION", "1d")]),
            "CF_ACCESS_SESSION_DURATION must use Cloudflare duration syntax, for example 24h",
        ),
        (
            with(base_env(), &[("CF_ACCESS_LOGIN_TEXT_COLOR", "#12345")]),
            "CF_ACCESS_LOGIN_TEXT_COLOR must be a six-digit hex color such as #142235",
        ),
        (
            // Login inputs are validated first, so this wins over the missing token.
            with(
                without(base_env(), "CF_API_TOKEN"),
                &[("CF_ACCESS_LOGIN_LOGO_URL", "http://cdn.example.com/x.png")],
            ),
            "CF_ACCESS_LOGIN_LOGO_URL must be a public HTTPS URL without embedded credentials",
        ),
        (
            with(base_env(), &[("CF_ACCESS_LOGIN_LOGO_URL", "logo.png")]),
            "CF_ACCESS_LOGIN_LOGO_URL must be a public HTTPS URL",
        ),
    ]
}

#[test]
fn configuration_is_validated_before_any_request() {
    for (env, message) in invalid_configurations() {
        let outcome = invoke(&Fake::fresh(), &["--apply"], &env);
        assert!(!outcome.ok, "{message}");
        assert!(outcome.requests.is_empty(), "{message}");
        assert_eq!(outcome.stdout, "", "{message}");
        assert_eq!(outcome.stderr, format!("Setup failed: {message}\n"));
    }
}

#[test]
fn help_and_unknown_arguments_never_call_cloudflare() {
    let outcome = invoke(&Fake::fresh(), &["--apply", "-h"], &base_env());
    assert!(outcome.ok);
    assert!(outcome.requests.is_empty());
    assert!(
        outcome
            .stdout
            .starts_with("Usage: cargo xtask cf-access-setup [--apply]\n\nDry-run is the default.")
    );

    let outcome = invoke(&Fake::fresh(), &["--apply", "--force"], &base_env());
    assert!(!outcome.ok);
    assert!(outcome.requests.is_empty());
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "Error: Unknown argument: --force\n");
}

#[test]
fn durations_follow_the_cloudflare_syntax() {
    for valid in [
        "24h", "1h30m", "90s", "15m", "300ms", "10us", "10µs", "5ns", "1h1h",
    ] {
        assert!(super::valid_duration(valid), "{valid}");
    }
    for invalid in ["", "h", "1", "1d", "1 h", "1.5h", "24H", "-1h", "1hm"] {
        assert!(!super::valid_duration(invalid), "{invalid}");
    }
}

/// Every case above, run through the former Node script and through the port.
#[test]
fn node_parity_when_the_former_script_is_available() {
    let Some(dir) = std::env::var_os("XTASK_NODE_PARITY_DIR") else {
        eprintln!("skipped: set XTASK_NODE_PARITY_DIR to compare with cf-access-setup.mjs");
        return;
    };
    let script = Path::new(&dir).join("cf-access-setup.mjs");
    assert!(script.is_file(), "{} is missing", script.display());
    let mut cases: Vec<(&str, Fake, Vec<&str>, Env)> = Vec::new();
    for apply in [vec![], vec!["--apply"]] {
        let (fake, env) = first_run_case();
        cases.push(("first run", fake, apply.clone(), env));
        let (fake, env) = drifted_case();
        cases.push(("drifted", fake, apply.clone(), env));
        let (fake, env) = converged_case();
        cases.push(("converged", fake.clone(), apply.clone(), env.clone()));
        cases.push(("no zone", fake, apply.clone(), without(env, "CF_ZONE_ID")));
        cases.push(("403", forbidden(), apply.clone(), base_env()));
        for (env, _) in invalid_configurations() {
            cases.push(("invalid configuration", Fake::fresh(), apply.clone(), env));
        }
    }
    let rewrite = "data:text/javascript,const f=globalThis.fetch;globalThis.fetch=(u,o)=>f(String(u).replace('https://api.cloudflare.com',process.env.PARITY_BASE),o);";
    for (name, fake, args, env) in cases {
        let rust = invoke(&fake, &args, &env);
        let scripted = fake.clone();
        let server = MockServer::start(move |request| scripted.reply(request));
        let output = Command::new("node")
            .arg("--import")
            .arg(rewrite)
            .arg(&script)
            .args(&args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PARITY_BASE", server.base())
            .envs(env.iter().copied())
            .output()
            .expect("run node");
        let node = Outcome {
            ok: output.status.success(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            requests: server.requests(),
        };
        assert_eq!(rust.ok, node.ok, "{name} {args:?}: exit status");
        assert_eq!(rust.stdout, node.stdout, "{name} {args:?}: stdout");
        assert_eq!(rust.stderr, node.stderr, "{name} {args:?}: stderr");
        assert_eq!(calls(&rust), calls(&node), "{name} {args:?}: requests");
        for (ours, theirs) in rust.requests.iter().zip(&node.requests) {
            for header in ["authorization", "content-type"] {
                assert_eq!(
                    ours.header(header),
                    theirs.header(header),
                    "{name}: {header}"
                );
            }
        }
        eprintln!(
            "node parity ok: {name} {args:?} ({} requests)",
            node.requests.len()
        );
    }
}
