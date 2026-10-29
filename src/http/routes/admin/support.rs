use std::time::Duration;

use axum::{
    extract::Request,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Number, Value};

use crate::{
    AppDeps,
    error::AppError,
    http::ingress::{
        BodyReadError, ViewerCost, complexity_response, read_body_limited, validate_json_complexity,
    },
    mcp::protocol::OrderedJson,
    model::Viewer,
    security::{
        access::AccessPolicy,
        audit::{AuditRequestId, MutationAudit},
    },
};
pub(crate) async fn require_admin(deps: &AppDeps, headers: &HeaderMap) -> Result<Viewer, Response> {
    let viewer = deps
        .viewer_identity
        .resolve(headers)
        .await
        .map_err(IntoResponse::into_response)?;
    AccessPolicy::admin_access(&viewer).map_err(IntoResponse::into_response)?;
    deps.mcp_telemetry.record_security_signal("admin_action");
    if !deps
        .ingress
        .allow_verified_viewer(headers, &viewer, ViewerCost::Admin)
    {
        return Err(AppError::RateLimited.into_response());
    }
    Ok(viewer)
}

pub(crate) async fn admin_json_request(
    deps: &AppDeps,
    request: Request,
) -> Result<(Viewer, Value, MutationAudit), Response> {
    let headers = request.headers().clone();
    let request_id = request.extensions().get::<AuditRequestId>().cloned();
    let viewer = require_admin(deps, &headers).await?;
    // Authenticate, authorize, and apply the verified-admin quota before admitting parser work.
    // The body reader remains bounded for authorized callers, but an unauthorized peer cannot
    // spend a request permit on a slow or structurally expensive JSON payload.
    let body = parse_json_body(request, deps.config.body.key_json, &deps.config.ingress).await?;
    let audit = MutationAudit::viewer_with_request_id(&viewer, request_id.as_ref())
        .map_err(IntoResponse::into_response)?;
    Ok((viewer, body, audit))
}

pub(super) fn target_audit(audit: MutationAudit, name: &str) -> Result<MutationAudit, AppError> {
    audit.admin_for_tenant(name)
}

async fn parse_json_body(
    request: Request,
    limit: u64,
    ingress: &crate::config::IngressConfig,
) -> Result<Value, Response> {
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json_content_type);
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let bytes = read_body_limited(
        request.into_body(),
        limit,
        Duration::from_millis(ingress.read_timeout_ms),
    )
    .await
    .map_err(|error| match error {
        BodyReadError::Timeout => {
            (axum::http::StatusCode::REQUEST_TIMEOUT, "request timeout").into_response()
        }
        BodyReadError::TooLarge => AppError::PayloadTooLarge.into_response(),
        BodyReadError::Invalid => AppError::Validation("invalid JSON".to_owned()).into_response(),
    })?;
    if !is_json {
        return if bytes.is_empty() {
            Ok(Value::Object(Map::new()))
        } else {
            Err((
                axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported media type",
            )
                .into_response())
        };
    }
    if bytes.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_json::from_slice::<OrderedJson>(&bytes) {
        Ok(value @ (OrderedJson::Object(_) | OrderedJson::Array(_))) => {
            validate_json_complexity(&value, ingress).map_err(complexity_response)?;
            Ok(value.into_value())
        }
        Ok(_) | Err(_) => Err(AppError::Validation("invalid JSON".to_owned()).into_response()),
    }
}

fn is_json_content_type(value: &str) -> bool {
    value
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
}

pub(super) fn js_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        Some(Value::Bool(true)) => "true".to_owned(),
        Some(Value::Number(value)) if value.as_f64() != Some(0.0) => js_number(value),
        Some(Value::Array(values)) if !values.is_empty() => values
            .iter()
            .map(js_array_element_string)
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_owned(),
        _ => String::new(),
    }
}

fn js_array_element_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => js_number(value),
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(js_array_element_string)
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

fn js_number(value: &Number) -> String {
    if let Some(integer) = value.as_i64() {
        return integer.to_string();
    }
    if let Some(integer) = value.as_u64() {
        return integer.to_string();
    }
    let Some(number) = value.as_f64() else {
        return "NaN".to_owned();
    };
    if number == 0.0 {
        return "0".to_owned();
    }
    let raw = serde_json::to_string(&number).unwrap_or_else(|_| "NaN".to_owned());
    let absolute = number.abs();
    if (1e-6..1e21).contains(&absolute) {
        decimal_notation(&raw)
    } else {
        exponent_notation(&raw)
    }
}

fn decimal_notation(raw: &str) -> String {
    let Some((mantissa, exponent)) = split_exponent(raw) else {
        return raw.strip_suffix(".0").unwrap_or(raw).to_owned();
    };
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.trim_start_matches('-');
    let mut digits = unsigned.replace('.', "");
    let decimal_at = i32::try_from(unsigned.find('.').unwrap_or(unsigned.len()))
        .unwrap_or(i32::MAX)
        .saturating_add(exponent);
    let rendered = if decimal_at <= 0 {
        format!(
            "0.{}{}",
            "0".repeat(usize::try_from(-decimal_at).unwrap_or(usize::MAX)),
            digits
        )
    } else if usize::try_from(decimal_at).map_or(true, |index| index >= digits.len()) {
        let decimal_at = usize::try_from(decimal_at).unwrap_or(usize::MAX);
        digits.push_str(&"0".repeat(decimal_at.saturating_sub(digits.len())));
        digits
    } else {
        let decimal_at = usize::try_from(decimal_at).unwrap_or_default();
        digits.insert(decimal_at, '.');
        digits
    };
    if negative {
        format!("-{rendered}")
    } else {
        rendered
    }
}

fn exponent_notation(raw: &str) -> String {
    let Some((mantissa, exponent)) = split_exponent(raw) else {
        return raw.strip_suffix(".0").unwrap_or(raw).to_owned();
    };
    let mantissa = mantissa.strip_suffix(".0").unwrap_or(mantissa);
    if exponent >= 0 {
        format!("{mantissa}e+{exponent}")
    } else {
        format!("{mantissa}e{exponent}")
    }
}

fn split_exponent(raw: &str) -> Option<(&str, i32)> {
    let position = raw.find(['e', 'E'])?;
    let exponent = raw.get(position + 1..)?.parse().ok()?;
    Some((&raw[..position], exponent))
}

pub(super) fn js_trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        matches!(character, '\u{feff}')
            || (character.is_whitespace() && !matches!(character, '\u{85}'))
    })
}

pub(super) fn iso_timestamp(unix_millis: i64) -> String {
    let seconds = unix_millis.div_euclid(1000);
    let millis = unix_millis.rem_euclid(1000);
    let moment = time::OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second(),
        millis
    )
}
