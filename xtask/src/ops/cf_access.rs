//! `cargo xtask cf-access-setup [--apply]`: bootstrap the Cloudflare Access catch-all
//! application, the account-wide login design and the zone Configuration Rule that disables
//! Email Obfuscation for the artifact host.
//!
//! Port of the former `scripts/cf-access-setup.mjs`. Dry-run (preview) is the default and only
//! reads from the Cloudflare API; `--apply` performs the create/update calls. The command never
//! creates, reads or changes an Access policy: those stay operator-owned.

use std::io::Write;

use sha2::{Digest, Sha256};
use url::Url;

use super::env_var;
use super::http::{FetchError, Http};
use super::js::{self, Js, Object, object};

pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

const USAGE: &str = "Usage: cargo xtask cf-access-setup [--apply]

Dry-run is the default. --apply creates or updates non-policy Cloudflare settings.

Required env:
  CF_API_TOKEN CF_ACCOUNT_ID PUBLIC_BASE_URL CF_ACCESS_IDP_ID

Optional env:
  CF_ZONE_ID CF_ACCESS_SESSION_DURATION
  CF_ACCESS_LOGIN_LOGO_URL CF_ACCESS_LOGIN_HEADER CF_ACCESS_LOGIN_FOOTER
  CF_ACCESS_LOGIN_BACKGROUND_COLOR CF_ACCESS_LOGIN_TEXT_COLOR";

/// Fields Cloudflare accepts back on `PUT /access/apps/{id}`.
const APP_UPDATE_FIELDS: &[&str] = &[
    "name",
    "domain",
    "type",
    "allow_authenticate_via_warp",
    "allow_iframe",
    "allowed_idps",
    "app_launcher_visible",
    "auto_redirect_to_identity",
    "cors_headers",
    "custom_deny_message",
    "custom_deny_url",
    "custom_non_identity_deny_url",
    "custom_pages",
    "destinations",
    "mfa_config",
    "options_preflight_bypass",
    "read_service_tokens_from_header",
    "service_auth_401_redirect",
    "session_duration",
    "skip_interstitial",
    "tags",
    "use_clientless_isolation_app_launcher_url",
];

/// Fields Cloudflare accepts back on `PUT /access/organizations`.
const ORGANIZATION_UPDATE_FIELDS: &[&str] = &[
    "allow_authenticate_via_warp",
    "auth_domain",
    "auto_redirect_to_identity",
    "custom_pages",
    "deny_unmatched_requests",
    "deny_unmatched_requests_exempted_zone_names",
    "is_ui_read_only",
    "login_design",
    "mfa_config",
    "mfa_piv_key_requirements",
    "mfa_required_for_all_apps",
    "name",
    "session_duration",
    "ui_read_only_toggle_reason",
    "user_seat_expiration_inactive_time",
    "warp_auth_session_duration",
];

/// Entry point for `cargo xtask cf-access-setup`.
pub fn main(args: &[String]) -> bool {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run(
        args,
        &env_var,
        API_BASE,
        &mut stdout.lock(),
        &mut stderr.lock(),
    )
}

/// Run the command against `api_base` with an injectable environment and output streams.
pub(crate) fn run(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    api_base: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    let mut unique: Vec<&str> = Vec::new();
    for arg in args {
        if !unique.contains(&arg.as_str()) {
            unique.push(arg);
        }
    }
    if unique.iter().any(|arg| *arg == "--help" || *arg == "-h") {
        let _ = writeln!(out, "{USAGE}");
        return true;
    }
    if let Some(arg) = unique.iter().find(|arg| **arg != "--apply") {
        let _ = writeln!(err, "Error: Unknown argument: {arg}");
        return false;
    }
    let apply = !unique.is_empty();
    let config = match read_config(env) {
        Ok(config) => config,
        Err(message) => {
            let _ = writeln!(err, "Setup failed: {message}");
            return false;
        }
    };
    let mut setup = Setup {
        apply,
        api_base,
        config,
        http: None,
        out,
        err,
    };
    match setup.main() {
        Ok(()) => true,
        Err(message) => {
            setup.warn(format_args!("Setup failed: {message}"));
            false
        }
    }
}

struct Config {
    api_token: String,
    account_id: String,
    hostname: String,
    idp_id: String,
    zone_id: String,
    session_duration: String,
    login_design: Object,
    organization: Option<Js>,
}

struct Setup<'a> {
    apply: bool,
    api_base: &'a str,
    config: Config,
    http: Option<Http>,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
}

/// `String(process.env[name] || fallback).trim()`.
fn env_value(env: &dyn Fn(&str) -> Option<String>, name: &str, fallback: &str) -> String {
    let raw = env(name).filter(|value| !value.is_empty());
    js::js_trim(raw.as_deref().unwrap_or(fallback)).to_owned()
}

fn required(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    let value = env_value(env, name, "");
    if value.is_empty() {
        return Err(format!("{name} is required"));
    }
    Ok(value)
}

fn has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some_and(|password| !password.is_empty())
}

fn public_hostname(value: &str) -> Result<String, String> {
    let url = Url::parse(value).map_err(|_| "PUBLIC_BASE_URL must be a valid HTTPS URL")?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || host.is_empty() || has_credentials(&url) {
        return Err(
            "PUBLIC_BASE_URL must be a public HTTPS URL without embedded credentials".to_owned(),
        );
    }
    let path = url.path();
    if (!path.is_empty() && path != "/")
        || url.query().is_some_and(|query| !query.is_empty())
        || url.fragment().is_some_and(|fragment| !fragment.is_empty())
    {
        return Err(
            "PUBLIC_BASE_URL must identify a hostname, without a path, query, or fragment"
                .to_owned(),
        );
    }
    Ok(host.to_lowercase())
}

/// `/^(?:\d+(?:ns|us|µs|ms|s|m|h))+$/`.
fn valid_duration(value: &str) -> bool {
    let mut rest = value;
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return false;
        }
        rest = &rest[digits..];
        let Some(unit) = ["ns", "us", "µs", "ms", "s", "m", "h"]
            .into_iter()
            .find(|unit| rest.starts_with(unit))
        else {
            return false;
        };
        rest = &rest[unit.len()..];
    }
    true
}

fn validate_duration(value: String) -> Result<String, String> {
    if !valid_duration(&value) {
        return Err(
            "CF_ACCESS_SESSION_DURATION must use Cloudflare duration syntax, for example 24h"
                .to_owned(),
        );
    }
    Ok(value)
}

fn validate_color(name: &str, value: String) -> Result<String, String> {
    let valid = value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit());
    if !value.is_empty() && !valid {
        return Err(format!(
            "{name} must be a six-digit hex color such as #142235"
        ));
    }
    Ok(value)
}

fn validate_logo(value: String) -> Result<String, String> {
    if value.is_empty() {
        return Ok(value);
    }
    let url =
        Url::parse(&value).map_err(|_| "CF_ACCESS_LOGIN_LOGO_URL must be a public HTTPS URL")?;
    if url.scheme() != "https"
        || url.host_str().unwrap_or_default().is_empty()
        || has_credentials(&url)
    {
        return Err(
            "CF_ACCESS_LOGIN_LOGO_URL must be a public HTTPS URL without embedded credentials"
                .to_owned(),
        );
    }
    Ok(value)
}

fn read_config(env: &dyn Fn(&str) -> Option<String>) -> Result<Config, String> {
    let entries = [
        (
            "logo_path",
            validate_logo(env_value(env, "CF_ACCESS_LOGIN_LOGO_URL", ""))?,
        ),
        ("header_text", env_value(env, "CF_ACCESS_LOGIN_HEADER", "")),
        ("footer_text", env_value(env, "CF_ACCESS_LOGIN_FOOTER", "")),
        (
            "background_color",
            validate_color(
                "CF_ACCESS_LOGIN_BACKGROUND_COLOR",
                env_value(env, "CF_ACCESS_LOGIN_BACKGROUND_COLOR", ""),
            )?,
        ),
        (
            "text_color",
            validate_color(
                "CF_ACCESS_LOGIN_TEXT_COLOR",
                env_value(env, "CF_ACCESS_LOGIN_TEXT_COLOR", ""),
            )?,
        ),
    ];
    let mut login_design = Object::new();
    for (key, value) in entries {
        if !value.is_empty() {
            login_design.insert(key, Js::String(value));
        }
    }
    Ok(Config {
        api_token: required(env, "CF_API_TOKEN")?,
        account_id: required(env, "CF_ACCOUNT_ID")?,
        hostname: public_hostname(&required(env, "PUBLIC_BASE_URL")?)?,
        idp_id: required(env, "CF_ACCESS_IDP_ID")?,
        zone_id: env_value(env, "CF_ZONE_ID", ""),
        session_duration: validate_duration(env_value(env, "CF_ACCESS_SESSION_DURATION", "24h"))?,
        login_design,
        organization: None,
    })
}

/// `pick(source, fields)`: copy the present fields, in field order.
fn pick(source: &Js, fields: &[&str]) -> Object {
    let mut picked = Object::new();
    for field in fields {
        if let Some(value) = source.property(field) {
            picked.insert(*field, value.clone());
        }
    }
    picked
}

/// Strip a scheme and trailing slashes, lowercase: compare Access targets by host.
fn normalized_target(value: Option<&Js>) -> String {
    let text = if js::truthy(value) {
        js::to_js_string(value)
    } else {
        String::new()
    };
    let lowered = js::js_trim(&text).to_lowercase();
    let without_scheme = lowered
        .strip_prefix("https://")
        .or_else(|| lowered.strip_prefix("http://"))
        .unwrap_or(&lowered);
    without_scheme.trim_end_matches('/').to_owned()
}

fn app_matches_hostname(app: &Js, hostname: &str) -> bool {
    if !js::truthy(Some(app)) || !js::is_str(app.property("type"), "self_hosted") {
        return false;
    }
    if normalized_target(app.property("domain")) == hostname {
        return true;
    }
    match app.property("destinations") {
        Some(Js::Array(destinations)) => destinations.iter().any(|destination| {
            js::is_str(destination.property("type"), "public")
                && normalized_target(destination.property("uri")) == hostname
        }),
        _ => false,
    }
}

fn rule_is_correct(current: &Js, desired: &Js) -> bool {
    js::strict_equals(current.property("action"), desired.property("action"))
        && js::strict_equals(
            current.property("expression"),
            desired.property("expression"),
        )
        && current.property("enabled") != Some(&Js::Bool(false))
        && js::same_value(
            &js::or(
                current.property("action_parameters"),
                Js::Object(Object::new()),
            ),
            desired.property("action_parameters").unwrap_or(&Js::Null),
        )
}

impl Setup<'_> {
    fn say(&mut self, line: impl std::fmt::Display) {
        let _ = writeln!(self.out, "{line}");
    }

    fn warn(&mut self, line: impl std::fmt::Display) {
        let _ = writeln!(self.err, "{line}");
    }

    fn account_path(&self, rest: &str) -> String {
        format!(
            "/accounts/{}{rest}",
            js::encode_uri_component(&self.config.account_id)
        )
    }

    fn zone_path(&self, rest: &str) -> String {
        format!(
            "/zones/{}{rest}",
            js::encode_uri_component(&self.config.zone_id)
        )
    }

    /// One Cloudflare API call; returns the parsed envelope or the operator-facing error.
    fn cf(&mut self, path: &str, method: &str, body: Option<&Js>) -> Result<Js, String> {
        if self.http.is_none() {
            self.http = Some(
                Http::new(None, true)
                    .map_err(|_| "Cloudflare API request failed: fetch failed".to_owned())?,
            );
        }
        let http = self.http.as_ref().expect("initialised above");
        let authorization = format!("Bearer {}", self.config.api_token);
        let mut headers: Vec<(&'static str, &str)> = vec![("authorization", &authorization)];
        if body.is_some() {
            headers.push(("content-type", "application/json"));
        }
        let response = http
            .send(
                method,
                &format!("{}{path}", self.api_base),
                &headers,
                body.map(|body| js::stringify(body).into_bytes()),
            )
            .map_err(|error| {
                let reason = match error {
                    FetchError::Timeout => "The operation was aborted due to timeout",
                    FetchError::Network => "fetch failed",
                    FetchError::InvalidHeader(_) => {
                        "the CF_API_TOKEN value is not a valid HTTP header value"
                    }
                };
                format!("Cloudflare API request failed: {reason}")
            })?;
        let text = response.text();
        let envelope = if text.is_empty() {
            Js::Object(Object::new())
        } else {
            js::parse(&text).map_err(|_| {
                format!(
                    "Cloudflare API {method} {path} returned non-JSON status {}",
                    response.status
                )
            })?
        };
        let failed =
            !response.ok() || js::get(Some(&envelope), "success")? == Some(&Js::Bool(false));
        if failed {
            let mut details = Vec::new();
            for key in ["errors", "messages"] {
                let entries = js::get(Some(&envelope), key)?;
                if let Some(Js::Array(entries)) = entries {
                    for entry in entries {
                        let parts: Vec<String> = ["code", "message"]
                            .into_iter()
                            .map(|field| entry.property(field))
                            .filter(|value| js::truthy(*value))
                            .map(js::to_js_string)
                            .collect();
                        let joined = parts.join(": ");
                        if !joined.is_empty() {
                            details.push(joined);
                        }
                    }
                }
            }
            let details = details.join("; ");
            let suffix = if details.is_empty() {
                String::new()
            } else {
                format!(": {details}")
            };
            return Err(format!(
                "Cloudflare API {method} {path} failed ({}){suffix}",
                response.status
            ));
        }
        Ok(envelope)
    }

    fn list_access_apps(&mut self) -> Result<Vec<Js>, String> {
        let mut applications = Vec::new();
        let mut page = 1_u64;
        loop {
            let path = self.account_path(&format!("/access/apps?per_page=100&page={page}"));
            let envelope = self.cf(&path, "GET", None)?;
            if let Some(Js::Array(items)) = envelope.property("result") {
                applications.extend(items.iter().cloned());
            }
            let pages = js::to_number(js::get_opt(envelope.property("result_info"), "total_pages"));
            let total_pages = if pages.is_nan() || pages == 0.0 {
                1.0
            } else {
                pages.max(1.0)
            };
            page += 1;
            #[allow(clippy::cast_precision_loss)]
            if page as f64 > total_pages {
                return Ok(applications);
            }
        }
    }

    fn ensure_access_app(&mut self) -> Result<Option<Js>, String> {
        let hostname = self.config.hostname.clone();
        let matches: Vec<Js> = self
            .list_access_apps()?
            .into_iter()
            .filter(|app| app_matches_hostname(app, &hostname))
            .collect();
        if matches.len() > 1 {
            return Err(format!(
                "More than one self-hosted Access application matches {hostname}; resolve the duplicate before continuing"
            ));
        }
        let idps = Js::Array(vec![Js::from(self.config.idp_id.as_str())]);
        let duration = Js::from(self.config.session_duration.as_str());

        let Some(current) = matches.into_iter().next() else {
            let desired = object([
                ("name", Js::from(format!("artifact-mcp ({hostname})"))),
                ("domain", Js::from(hostname.as_str())),
                ("type", Js::from("self_hosted")),
                (
                    "destinations",
                    Js::Array(vec![object([
                        ("type", Js::from("public")),
                        ("uri", Js::from(hostname.as_str())),
                    ])]),
                ),
                ("allowed_idps", idps),
                ("auto_redirect_to_identity", Js::Bool(true)),
                ("app_launcher_visible", Js::Bool(false)),
                ("session_duration", duration),
            ]);
            if !self.apply {
                self.say(format_args!(
                    "[dry-run] Access application: would create catch-all {hostname}"
                ));
                return Ok(None);
            }
            let path = self.account_path("/access/apps");
            let envelope = self.cf(&path, "POST", Some(&desired))?;
            self.say(format_args!("Access application: created {hostname}"));
            return Ok(envelope.property("result").cloned());
        };

        let id = js::to_js_string(current.property("id"));
        let correct = current.property("auto_redirect_to_identity") == Some(&Js::Bool(true))
            && js::same_value(
                &js::or(current.property("allowed_idps"), Js::Array(Vec::new())),
                &idps,
            )
            && js::strict_equals(current.property("session_duration"), Some(&duration));
        if correct {
            self.say(format_args!("Access application: no-op ({id})"));
            return Ok(Some(current));
        }

        let mut desired = pick(&current, APP_UPDATE_FIELDS);
        let domain = js::or(current.property("domain"), Js::from(hostname.as_str()));
        desired.insert("domain", domain);
        desired.insert("type", Js::from("self_hosted"));
        desired.insert("allowed_idps", idps);
        desired.insert("auto_redirect_to_identity", Js::Bool(true));
        desired.insert("session_duration", duration);
        if !self.apply {
            self.say(format_args!(
                "[dry-run] Access application: would enable direct single-IdP login on {id}"
            ));
            return Ok(Some(current));
        }
        let path = self.account_path(&format!("/access/apps/{}", js::encode_uri_component(&id)));
        let envelope = self.cf(&path, "PUT", Some(&Js::Object(desired)))?;
        self.say(format_args!("Access application: updated {id}"));
        Ok(envelope.property("result").cloned())
    }

    fn get_organization(&mut self) -> Result<Js, String> {
        let path = self.account_path("/access/organizations");
        let envelope = self.cf(&path, "GET", None)?;
        match envelope.property("result") {
            Some(result @ (Js::Object(_) | Js::Array(_))) => Ok(result.clone()),
            _ => Err(
                "Cloudflare did not return a Zero Trust organization for this account".to_owned(),
            ),
        }
    }

    fn ensure_login_design(&mut self, organization: Option<Js>) -> Result<Option<Js>, String> {
        if self.config.login_design.is_empty() {
            self.say("Account-wide login design: no inputs supplied; skipped");
            return Ok(organization);
        }
        let organization_value = organization.clone().unwrap_or(Js::Null);
        let current = js::or(
            js::get(Some(&organization_value), "login_design")?,
            Js::Object(Object::new()),
        );
        let mut desired = match &current {
            Js::Object(current) => current.clone(),
            _ => Object::new(),
        };
        for (key, value) in self.config.login_design.iter() {
            desired.insert(key, value.clone());
        }
        let desired = Js::Object(desired);
        self.say("Account-wide login design (affects every Access application):");
        let dump = js::stringify_pretty(&object([
            ("current", current.clone()),
            ("desired", desired.clone()),
        ]));
        self.say(dump);
        if js::same_value(&current, &desired) {
            self.say("Account-wide login design: no-op");
            return Ok(organization);
        }
        if !self.apply {
            self.say(
                "[dry-run] Account-wide login design: would update reusable Access Custom Page branding",
            );
            return Ok(organization);
        }
        let mut body = pick(&organization_value, ORGANIZATION_UPDATE_FIELDS);
        body.insert("login_design", desired);
        let path = self.account_path("/access/organizations");
        let envelope = self.cf(&path, "PUT", Some(&Js::Object(body)))?;
        self.say("Account-wide login design: updated");
        Ok(envelope.property("result").cloned())
    }

    fn configuration_rule(&self) -> Js {
        let hostname = &self.config.hostname;
        let digest = hex::encode(Sha256::digest(hostname.as_bytes()));
        object([
            (
                "ref",
                Js::from(format!("artifact_mcp_email_off_{}", &digest[..12])),
            ),
            (
                "expression",
                Js::from(format!(
                    "http.host eq {}",
                    js::stringify(&Js::from(hostname.as_str()))
                )),
            ),
            (
                "description",
                Js::from(format!(
                    "artifact-mcp: disable Email Obfuscation for {hostname}"
                )),
            ),
            ("action", Js::from("set_config")),
            (
                "action_parameters",
                object([("email_obfuscation", Js::Bool(false))]),
            ),
            ("enabled", Js::Bool(true)),
        ])
    }

    fn ensure_configuration_rule(&mut self) {
        if self.config.zone_id.is_empty() {
            self.warn(
                "Configuration Rule: skipped (CF_ZONE_ID is unset; origin no-transform remains active)",
            );
            return;
        }
        let desired = self.configuration_rule();
        if let Err(message) = self.try_configuration_rule(&desired) {
            self.warn(format_args!(
                "Configuration Rule: skipped ({message}; origin no-transform remains active)"
            ));
        }
    }

    fn try_configuration_rule(&mut self, desired: &Js) -> Result<(), String> {
        let hostname = self.config.hostname.clone();
        let path = self.zone_path("/rulesets?per_page=100");
        let listed = self.cf(&path, "GET", None)?;
        let mut phase_rulesets = Vec::new();
        if let Some(Js::Array(rulesets)) = listed.property("result") {
            for ruleset in rulesets {
                if js::is_str(js::get(Some(ruleset), "kind")?, "zone")
                    && js::is_str(js::get(Some(ruleset), "phase")?, "http_config_settings")
                {
                    phase_rulesets.push(ruleset.clone());
                }
            }
        }
        if phase_rulesets.len() > 1 {
            return Err(
                "multiple zone entry-point rulesets exist for http_config_settings".to_owned(),
            );
        }

        let Some(phase_ruleset) = phase_rulesets.first() else {
            if !self.apply {
                self.say(format_args!(
                    "[dry-run] Configuration Rule: would create for {hostname}"
                ));
                return Ok(());
            }
            let body = object([
                ("name", Js::from("artifact-mcp configuration rules")),
                (
                    "description",
                    Js::from("Host-specific response transformation safeguards for artifact-mcp"),
                ),
                ("kind", Js::from("zone")),
                ("phase", Js::from("http_config_settings")),
                ("rules", Js::Array(vec![desired.clone()])),
            ]);
            let path = self.zone_path("/rulesets");
            self.cf(&path, "POST", Some(&body))?;
            self.say("Configuration Rule: created with the phase ruleset");
            return Ok(());
        };

        let ruleset_id = js::to_js_string(phase_ruleset.property("id"));
        let path = self.zone_path(&format!(
            "/rulesets/{}",
            js::encode_uri_component(&ruleset_id)
        ));
        let fetched = self.cf(&path, "GET", None)?;
        let ruleset = fetched.property("result");
        let rules = js::get(ruleset, "rules")?;
        let mut current = None;
        if let Some(Js::Array(rules)) = rules.filter(|rules| js::truthy(Some(rules))) {
            for rule in rules {
                let reference = js::get(Some(rule), "ref")?;
                if js::strict_equals(reference, desired.property("ref"))
                    || js::strict_equals(
                        js::get(Some(rule), "description")?,
                        desired.property("description"),
                    )
                {
                    current = Some(rule.clone());
                    break;
                }
            }
        }
        if let Some(rule) = &current
            && rule_is_correct(rule, desired)
        {
            self.say("Configuration Rule: no-op");
            return Ok(());
        }
        if !self.apply {
            let verb = if current.is_some() { "update" } else { "add" };
            self.say(format_args!(
                "[dry-run] Configuration Rule: would {verb} for {hostname}"
            ));
            return Ok(());
        }
        let ruleset_id = js::encode_uri_component(&js::to_js_string(js::get(ruleset, "id")?));
        if let Some(rule) = current {
            let rule_id = js::encode_uri_component(&js::to_js_string(rule.property("id")));
            let path = self.zone_path(&format!("/rulesets/{ruleset_id}/rules/{rule_id}"));
            self.cf(&path, "PATCH", Some(desired))?;
            self.say("Configuration Rule: updated");
        } else {
            let path = self.zone_path(&format!("/rulesets/{ruleset_id}/rules"));
            self.cf(&path, "POST", Some(desired))?;
            self.say("Configuration Rule: added");
        }
        Ok(())
    }

    fn operator_actions(&mut self, app: Option<&Js>) {
        let hostname = self.config.hostname.clone();
        self.say("\nRuntime values:");
        let aud = js::get_opt(app, "aud");
        if js::truthy(aud) {
            self.say(format_args!("CF_ACCESS_AUD={}", js::to_js_string(aud)));
        } else if !self.apply {
            self.say("CF_ACCESS_AUD=<created by a subsequent --apply run>");
        }
        let auth_domain = js::get_opt(self.config.organization.as_ref(), "auth_domain");
        if js::truthy(auth_domain) {
            let line = format!("CF_ACCESS_TEAM_DOMAIN={}", js::to_js_string(auth_domain));
            self.say(line);
        } else {
            self.warn(
                "CF_ACCESS_TEAM_DOMAIN was not returned; copy the organization auth domain from Zero Trust",
            );
        }
        let app_id = js::get_opt(app, "id");
        let app_suffix = if js::truthy(app_id) {
            format!(" ({})", js::to_js_string(app_id))
        } else {
            String::new()
        };
        self.say(format_args!(
            "\nRemaining operator-owned security actions (this script never changes policies):
  1. Create or retain an Access application for {hostname}/mcp with Bypass -> Everyone.
  2. Create or retain an Access application for {hostname}/s/* with Bypass -> Everyone.
  3. Create or retain the intended-viewer Allow policy on the catch-all {hostname} app{app_suffix}.
  4. Set CF_ACCESS_AUD, CF_ACCESS_TEAM_DOMAIN, and REQUIRE_ACCESS_JWT=1 in the runtime environment.
  5. Fully restart artifact-mcp; editing an env file does not update module-initialized identity settings."
        ));
    }

    fn main(&mut self) -> Result<(), String> {
        let mode = if self.apply { "apply" } else { "dry-run" };
        self.say(format_args!(
            "artifact-mcp Cloudflare Access setup ({mode})"
        ));
        let hostname = self.config.hostname.clone();
        self.say(format_args!("Hostname: {hostname}"));
        self.config.organization = Some(self.get_organization()?);
        let app = self.ensure_access_app()?;
        let organization = self.config.organization.take();
        self.config.organization = self.ensure_login_design(organization)?;
        self.ensure_configuration_rule();
        self.operator_actions(app.as_ref());
        Ok(())
    }
}

#[cfg(test)]
#[path = "cf_access_tests.rs"]
mod tests;
