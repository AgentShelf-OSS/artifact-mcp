//! Authorization-gated current and historical artifact delivery routes.

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
    routing::get,
};

use crate::{
    AppDeps,
    artifacts::validation::posix_normalize_relative,
    error::AppError,
    http::{
        artifact_response::{ArtifactResponseOptions, artifact_response},
        routes::gallery::{found_redirect, page_error_response, resolve_page_artifact},
    },
};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/raw/{id}/rev/{revision}/{*path}", get(revision_file))
        .route("/raw/{id}/rev/{revision}/", get(revision_entry))
        .route("/raw/{id}/rev/{revision}", get(revision_root))
        .route("/raw/{id}/{*path}", get(current_file))
        .route("/raw/{id}/", get(current_entry))
        .route("/raw/{id}", get(current_root))
}

async fn current_root(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    match current_root_result(&deps, &headers, &id, query.as_deref()).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn current_root_result(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
    query: Option<&str>,
) -> Result<Response, AppError> {
    let (_viewer, artifact) = resolve_page_artifact(deps, headers, id).await?;
    if artifact.meta().is_bundle {
        return found_redirect(&format!("/raw/{id}/"), false);
    }
    let Some(file) = deps.artifacts.read_body(&artifact).await? else {
        return Err(AppError::ConcealedNotFound);
    };
    let parsed = QueryValues::parse(query);
    artifact_response(
        file,
        ArtifactResponseOptions {
            anchor: parsed.single("anchor").as_deref() == Some("1"),
            preview: parsed.has("preview"),
            download_title: parsed
                .has("download")
                .then_some(artifact.meta().title.as_str()),
            ..ArtifactResponseOptions::default()
        },
    )
}

async fn current_file(
    State(deps): State<AppDeps>,
    Path((id, path)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    match current_file_result(&deps, &headers, &id, &path, query.as_deref()).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn current_entry(
    State(deps): State<AppDeps>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    match current_file_result(&deps, &headers, &id, "", query.as_deref()).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn current_file_result(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
    path: &str,
    query: Option<&str>,
) -> Result<Response, AppError> {
    let (_viewer, artifact) = resolve_page_artifact(deps, headers, id).await?;
    if !artifact.meta().is_bundle {
        return Err(AppError::ConcealedNotFound);
    }
    let path = express_wildcard(path);
    let Some(file) = deps.artifacts.read_bundle_file(&artifact, path).await? else {
        return Err(AppError::ConcealedNotFound);
    };
    let parsed = QueryValues::parse(query);
    let page_path = if path.is_empty() {
        artifact.meta().entry.clone()
    } else {
        posix_normalize_relative(&path.replace('\\', "/"))
    };
    artifact_response(
        file,
        ArtifactResponseOptions {
            anchor: parsed.single("anchor").as_deref() == Some("1") && !parsed.has("download"),
            preview: parsed.has("preview"),
            page_path: Some(&page_path),
            ..ArtifactResponseOptions::default()
        },
    )
}

async fn revision_root(
    State(deps): State<AppDeps>,
    Path((id, revision)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match revision_root_result(&deps, &headers, &id, &revision).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn revision_root_result(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
    raw_revision: &str,
) -> Result<Response, AppError> {
    let (_viewer, artifact) = resolve_page_artifact(deps, headers, id).await?;
    let parsed = JsNumber::parse(raw_revision);
    if artifact.meta().is_bundle {
        return found_redirect(&format!("/raw/{id}/rev/{}/", parsed.display()), false);
    }
    let Some(revision) = parsed.as_u64() else {
        return Err(AppError::ConcealedNotFound);
    };
    let Some(file) = deps
        .artifacts
        .read_revision_body(&artifact, revision, None)
        .await?
    else {
        return Err(AppError::ConcealedNotFound);
    };
    artifact_response(file, ArtifactResponseOptions::default())
}

async fn revision_file(
    State(deps): State<AppDeps>,
    Path((id, revision, path)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    match revision_file_result(&deps, &headers, &id, &revision, &path).await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn revision_entry(
    State(deps): State<AppDeps>,
    Path((id, revision)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match revision_file_result(&deps, &headers, &id, &revision, "").await {
        Ok(response) => response,
        Err(error) => page_error_response(&deps, error),
    }
}

async fn revision_file_result(
    deps: &AppDeps,
    headers: &HeaderMap,
    id: &str,
    raw_revision: &str,
    path: &str,
) -> Result<Response, AppError> {
    let (_viewer, artifact) = resolve_page_artifact(deps, headers, id).await?;
    let Some(revision) = JsNumber::parse(raw_revision).as_u64() else {
        return Err(AppError::ConcealedNotFound);
    };
    let path = express_wildcard(path);
    let Some(file) = deps
        .artifacts
        .read_revision_body(&artifact, revision, Some(path))
        .await?
    else {
        return Err(AppError::ConcealedNotFound);
    };
    artifact_response(file, ArtifactResponseOptions::default())
}

fn express_wildcard(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

#[derive(Default)]
pub(crate) struct QueryValues(Vec<(String, String)>);

impl QueryValues {
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        let pairs = raw
            .unwrap_or_default()
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (form_decode(key), form_decode(value))
            })
            .collect();
        Self(pairs)
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(candidate, _)| candidate == key)
    }

    pub(crate) fn single(&self, key: &str) -> Option<String> {
        let mut values = self
            .0
            .iter()
            .filter(|(candidate, _)| candidate == key)
            .map(|(_, value)| value);
        let value = values.next()?.clone();
        values.next().is_none().then_some(value)
    }
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        decoded.push((high << 4) | low);
                        index += 3;
                    }
                    _ => {
                        decoded.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

struct JsNumber(f64);

impl JsNumber {
    fn parse(value: &str) -> Self {
        let value = js_trim(value);
        let number = if value.is_empty() {
            0.0
        } else if let Some(hex) = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            u64::from_str_radix(hex, 16).map_or(f64::NAN, |number| number as f64)
        } else if let Some(binary) = value
            .strip_prefix("0b")
            .or_else(|| value.strip_prefix("0B"))
        {
            u64::from_str_radix(binary, 2).map_or(f64::NAN, |number| number as f64)
        } else if let Some(octal) = value
            .strip_prefix("0o")
            .or_else(|| value.strip_prefix("0O"))
        {
            u64::from_str_radix(octal, 8).map_or(f64::NAN, |number| number as f64)
        } else {
            value.parse::<f64>().unwrap_or(f64::NAN)
        };
        Self(number)
    }

    fn as_u64(&self) -> Option<u64> {
        (self.0.is_finite() && self.0 >= 0.0 && self.0.fract() == 0.0 && self.0 <= u64::MAX as f64)
            .then_some(self.0 as u64)
    }

    fn display(&self) -> String {
        if self.0.is_nan() {
            return "NaN".to_owned();
        }
        if self.0 == f64::INFINITY {
            return "Infinity".to_owned();
        }
        if self.0 == f64::NEG_INFINITY {
            return "-Infinity".to_owned();
        }
        if self.0 == 0.0 {
            return "0".to_owned();
        }
        let absolute = self.0.abs();
        if !(1.0e-6..1.0e21).contains(&absolute) {
            let scientific = format!("{:e}", self.0);
            let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
            let exponent = exponent.parse::<i32>().unwrap_or(0);
            return format!(
                "{mantissa}e{}{exponent}",
                if exponent >= 0 { "+" } else { "" }
            );
        }
        self.0.to_string()
    }
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'
                | '\u{000a}'
                | '\u{000b}'
                | '\u{000c}'
                | '\u{000d}'
                | '\u{0020}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}
