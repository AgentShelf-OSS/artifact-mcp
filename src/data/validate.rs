//! Source, parameter, and binding validation, and upstream request building.
use super::{
    BindingManifest, MAX_BINDINGS, MAX_BYTES, MAX_POST_BODY, OperationConfig, ParamConfig,
    SourceConfig,
};
use reqwest::{Url, header::HeaderName};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;

pub(super) fn valid_event_text(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
}
pub(super) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
pub(super) fn valid_path(s: &str) -> bool {
    s.starts_with('/')
        && !s.starts_with("//")
        && !s.contains('\\')
        && !s.contains('?')
        && !s.contains('#')
        && !s.chars().any(char::is_control)
        && !s.split('/').any(|p| p == "." || p == "..")
        && !s.to_ascii_lowercase().contains("%2e")
        && !s.to_ascii_lowercase().contains("%2f")
        && !s.to_ascii_lowercase().contains("%5c")
}
/// Prefix of every environment variable that an administration-managed connection may read.
///
/// Operator connections in `ARTIFACT_DATA_SOURCES_FILE` may name any variable. An administrator
/// may not: the connection also chooses the upstream host, so an unrestricted name would let an
/// administrator send any process secret to a host they control.
pub(super) const MANAGED_CREDENTIAL_ENV_PREFIX: &str = "ARTIFACT_DATA_CREDENTIAL_";

/// Headers that a credential reference may never set, for operator and managed connections alike.
pub(super) const RESERVED_CREDENTIAL_HEADERS: [&str; 5] = [
    "host",
    "cookie",
    "connection",
    "content-length",
    "transfer-encoding",
];

/// Whether an administration-managed connection may read the variable `env`: the
/// [`MANAGED_CREDENTIAL_ENV_PREFIX`] followed by one or more of `A-Z`, `0-9`, and `_`.
pub(super) fn managed_credential_name_allowed(env: &str) -> bool {
    env.strip_prefix(MANAGED_CREDENTIAL_ENV_PREFIX)
        .is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Whether an administration-managed connection may send `header` with the value of `env`.
pub(super) fn managed_credential_allowed(header: &str, env: &str) -> bool {
    managed_credential_name_allowed(env)
        && HeaderName::from_bytes(header.as_bytes())
            .is_ok_and(|name| !RESERVED_CREDENTIAL_HEADERS.contains(&name.as_str()))
}

/// The names in `names` that a managed connection may reference, sorted and without duplicates.
/// The administration page offers them as choices. It takes names only, so no value can reach
/// the page.
pub(super) fn managed_credential_names(names: impl IntoIterator<Item = OsString>) -> Vec<String> {
    let mut allowed: Vec<String> = names
        .into_iter()
        .filter_map(|name| name.into_string().ok())
        .filter(|name| managed_credential_name_allowed(name))
        .collect();
    allowed.sort_unstable();
    allowed.dedup();
    allowed
}

/// Rejects a managed connection definition whose credential references break
/// [`managed_credential_allowed`]. Runs when an administrator saves, not at startup, so a
/// connection saved before the rule loads and reports `config_error` instead.
pub(super) fn validate_managed_credentials(s: &SourceConfig) -> Result<(), String> {
    if s.headers_env
        .iter()
        .all(|(header, env)| managed_credential_allowed(header, env))
    {
        Ok(())
    } else {
        Err("credential_reference_not_allowed".into())
    }
}

pub(super) fn validate_source(s: &SourceConfig) -> Result<(), String> {
    let error = || "invalid_data_sources".to_owned();
    if !valid_name(&s.id)
        || s.org.trim().is_empty()
        || !matches!(s.kind.as_str(), "http" | "push")
        || s.operations.len() > 16
        || s.subscriptions.len() > 16
    {
        return Err(error());
    }
    if s.kind == "http" {
        let u = Url::parse(s.base_url.as_deref().ok_or_else(error)?).map_err(|_| error())?;
        if !matches!(u.scheme(), "http" | "https")
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err(error());
        }
    } else if s.base_url.is_some() || !s.headers_env.is_empty() {
        return Err(error());
    }
    validate_operations(s, &error)?;
    validate_subscriptions(s, &error)
}

fn validate_operations(s: &SourceConfig, error: &dyn Fn() -> String) -> Result<(), String> {
    for (n, o) in &s.operations {
        let post = o.method == "POST";
        let max_timeout = if o.stream { 120000 } else { 30000 };
        if !valid_name(n)
            || !matches!(o.method.as_str(), "GET" | "POST")
            || s.kind == "push" && (post || o.stream)
            || o.max_bytes == 0
            || o.max_bytes > MAX_BYTES
            || !(100..=max_timeout).contains(&o.timeout_ms)
            || o.params.len() > if post { 32 } else { 16 }
        {
            return Err(error());
        }
        if s.kind == "push" {
            if o.path.is_some()
                || !o.params.is_empty()
                || o.key.as_ref().is_some_and(|k| !valid_name(k))
            {
                return Err(error());
            }
            continue;
        }
        if o.key.is_some() {
            return Err(error());
        }
        let path = o
            .path
            .as_deref()
            .filter(|p| valid_path(p))
            .ok_or_else(error)?;
        let mut remaining = path.to_owned();
        for (p, c) in &o.params {
            if !valid_name(p)
                || !matches!(c.kind.as_str(), "string" | "integer" | "boolean")
                || c.max_length
                    .is_some_and(|m| m == 0 || m > if post { 16384 } else { 256 })
                || c.minimum.zip(c.maximum).is_some_and(|(a, b)| a > b)
            {
                return Err(error());
            }
            if c.kind != "string" && c.max_length.is_some()
                || c.kind != "integer" && (c.minimum.is_some() || c.maximum.is_some())
            {
                return Err(error());
            }
            let marker = format!("{{{p}}}");
            if remaining.contains(&marker) {
                if c.kind != "string" || !c.required {
                    return Err(error());
                }
                remaining = remaining.replace(&marker, "");
            }
            if let Some(values) = &c.enum_values
                && (values.is_empty()
                    || values.len() > 100
                    || values.iter().any(|v| validate_param(c, v, false).is_err()))
            {
                return Err(error());
            }
            if let Some(v) = &c.default {
                validate_param(c, v, true).map_err(|_| error())?;
            }
        }
        if remaining.contains('{') || remaining.contains('}') {
            return Err(error());
        }
    }
    Ok(())
}

fn validate_subscriptions(s: &SourceConfig, error: &dyn Fn() -> String) -> Result<(), String> {
    for (n, sub) in &s.subscriptions {
        if !valid_name(n)
            || sub.events.len() > 16
            || sub.events.iter().any(|e| !valid_event_text(e))
        {
            return Err(error());
        }
        match sub.transport.as_str() {
            "push" if s.kind == "push" => {
                if sub.path.is_some() || sub.operation.is_some() || !sub.events.is_empty() {
                    return Err(error());
                }
            }
            "sse" if s.kind == "http" => {
                if !sub
                    .path
                    .as_deref()
                    .is_some_and(|p| valid_path(p) && !p.contains('{') && !p.contains('}'))
                    || sub.operation.is_some()
                {
                    return Err(error());
                }
            }
            "poll" if s.kind == "http" => {
                let op = sub
                    .operation
                    .as_ref()
                    .and_then(|n| s.operations.get(n))
                    .ok_or_else(error)?;
                if sub.path.is_some()
                    || !sub.events.is_empty()
                    || !(1000..=60000).contains(&sub.interval_ms)
                    || op
                        .params
                        .values()
                        .any(|p| p.required && p.default.is_none())
                {
                    return Err(error());
                }
            }
            _ => return Err(error()),
        }
    }
    Ok(())
}
pub(super) fn validate_param(
    c: &ParamConfig,
    v: &Value,
    check_enum: bool,
) -> Result<String, String> {
    let value = match c.kind.as_str() {
        "string" => {
            let s = v.as_str().ok_or("bad_params")?;
            if s.chars().count() > c.max_length.unwrap_or(256) {
                return Err("bad_params".into());
            }
            s.to_owned()
        }
        "integer" => {
            let i = v.as_i64().ok_or("bad_params")?;
            if c.minimum.is_some_and(|m| i < m) || c.maximum.is_some_and(|m| i > m) {
                return Err("bad_params".into());
            }
            i.to_string()
        }
        "boolean" => v.as_bool().ok_or("bad_params")?.to_string(),
        _ => return Err("bad_params".into()),
    };
    if check_enum && c.enum_values.as_ref().is_some_and(|e| !e.contains(v)) {
        return Err("bad_params".into());
    }
    Ok(value)
}
pub(super) fn validate_bindings(
    m: &BindingManifest,
    sources: &BTreeMap<String, SourceConfig>,
    org: &str,
) -> Result<(), String> {
    if m.bindings.len() > MAX_BINDINGS {
        return Err("bad_params".into());
    }
    for (n, b) in &m.bindings {
        let s = sources
            .get(&b.source)
            .filter(|s| s.org == org)
            .ok_or("bad_params")?;
        if !valid_name(n)
            || b.operations.iter().collect::<HashSet<_>>().len() != b.operations.len()
            || b.subscriptions.iter().collect::<HashSet<_>>().len() != b.subscriptions.len()
            || b.operations.iter().any(|n| !s.operations.contains_key(n))
            || b.subscriptions
                .iter()
                .any(|n| !s.subscriptions.contains_key(n))
        {
            return Err("bad_params".into());
        }
    }
    Ok(())
}
pub(super) fn build_url(
    s: &SourceConfig,
    o: &OperationConfig,
    params: &Value,
) -> Result<Url, String> {
    build_request(s, o, params).map(|(url, _)| url)
}
/// Builds the upstream URL and, for POST operations, the typed JSON body (ADR-0017).
pub(super) fn build_request(
    s: &SourceConfig,
    o: &OperationConfig,
    params: &Value,
) -> Result<(Url, Option<Value>), String> {
    let object = params.as_object().ok_or("bad_params")?;
    if object.keys().any(|p| !o.params.contains_key(p)) {
        return Err("bad_params".into());
    }
    let mut url = Url::parse(s.base_url.as_deref().ok_or("data_unavailable")?)
        .map_err(|_| "data_unavailable")?;
    let mut path = o.path.clone().ok_or("data_unavailable")?;
    let original = path.clone();
    let mut query = Vec::new();
    let post = o.method == "POST";
    let mut body = serde_json::Map::new();
    for (p, c) in &o.params {
        let Some(v) = object.get(p).or(c.default.as_ref()) else {
            if c.required {
                return Err("bad_params".into());
            }
            continue;
        };
        let value = validate_param(c, v, true)?;
        let marker = format!("{{{p}}}");
        if original.contains(&marker) {
            if value == "."
                || value == ".."
                || value.contains('/')
                || value.contains('\\')
                || value.chars().any(char::is_control)
            {
                return Err("bad_params".into());
            }
            let encoded: String = value
                .as_bytes()
                .iter()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(b) {
                        (*b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            path = path.replace(&marker, &encoded);
        } else if post {
            body.insert(p.clone(), v.clone());
        } else {
            query.push((p, value));
        }
    }
    url.set_path(&format!("{}{}", url.path().trim_end_matches('/'), path));
    for (p, v) in query {
        url.query_pairs_mut().append_pair(p, &v);
    }
    if !post {
        return Ok((url, None));
    }
    let body = Value::Object(body);
    if serde_json::to_vec(&body).map_err(|_| "bad_params")?.len() > MAX_POST_BODY {
        return Err("too_large".into());
    }
    Ok((url, Some(body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_credential_names_lists_only_referenceable_names_in_order() {
        let names = [
            "PATH",
            "DISCORD_BOT_TOKEN",
            "ARTIFACT_DATA_CREDENTIAL_ZED",
            "ARTIFACT_DATA_CREDENTIAL_",
            "ARTIFACT_DATA_CREDENTIAL_lower",
            "artifact_data_credential_LOWER",
            "ARTIFACT_DATA_CREDENTIAL_METRICS_2",
            "ARTIFACT_DATA_CREDENTIAL_ZED",
        ]
        .map(OsString::from);
        assert_eq!(
            managed_credential_names(names),
            [
                "ARTIFACT_DATA_CREDENTIAL_METRICS_2",
                "ARTIFACT_DATA_CREDENTIAL_ZED"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_credential_names_skips_non_utf8_names_without_panicking() {
        use std::os::unix::ffi::OsStringExt;
        let names = [
            OsString::from_vec(b"ARTIFACT_DATA_CREDENTIAL_\xff".to_vec()),
            OsString::from("ARTIFACT_DATA_CREDENTIAL_OK"),
        ];
        assert_eq!(
            managed_credential_names(names),
            ["ARTIFACT_DATA_CREDENTIAL_OK"]
        );
    }
}
