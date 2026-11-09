//! Raw artifact response policy.
//!
//! Artifact bytes are untrusted, agent-authored content. Every response built here receives the
//! exact CSP sandbox and egress policy, including non-HTML bundle assets
//! and downloads.
//! The sandbox deliberately omits `allow-same-origin` so direct navigation cannot regain the
//! application's origin.

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, header},
    response::Response,
};

use crate::{error::AppError, model::ArtifactFile};

/// Exact document policy fixed by ADR-0003.
pub const DOCUMENT_SANDBOX: &str = "sandbox allow-scripts allow-popups allow-forms allow-modals; default-src 'none'; connect-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: blob: https://fonts.gstatic.com; img-src 'self' data: blob:; media-src 'self' data: blob:; worker-src 'self' blob:";

/// Stable marker used to prove that anchor mode adds exactly one server-owned bridge.
pub const ANCHOR_BRIDGE_MARKER: &str = "artifact-anchor-bridge";
pub const READER_BRIDGE_MARKER: &str = "artifact-reader-bridge";
pub const READER_BRIDGE: &str = concat!(
    include_str!("../../assets/reader-ereader.js"),
    "\n",
    include_str!("../../assets/reader-bridge.js")
);
pub const DATA_CLIENT: &str = include_str!("../../assets/artifact-data-client.js");

/// Server-owned anchor bridge copied byte-for-byte.
///
/// Keep this asset static: artifact content must never be interpolated into the script. The
/// only variable portion is the optional page path, which [`inject_anchor_bridge`] JSON-encodes and
/// escapes before replacing the fixed `page=null` initializer.
pub const ANCHOR_BRIDGE: &str = include_str!("../../assets/artifact-anchor-bridge.html");

const PRIVATE_CACHE_CONTROL: &str = "private, max-age=60";
const PUBLIC_SHARE_CACHE_CONTROL: &str = "no-store";
const PAGE_INITIALIZER: &str = "var d=document,w=window,page=null";

/// Cache policy for the raw response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RawCachePolicy {
    /// Signed-in raw/history delivery: browser-private, one-minute cache.
    #[default]
    Private,
    /// Token-gated public share delivery: immediately revocable and not indexable.
    PublicShare,
}

/// Representation switches applied after the caller has resolved and read an [`ArtifactFile`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArtifactResponseOptions<'a> {
    /// Add the anchor bridge to HTML unless this is also a download representation.
    pub anchor: bool,
    /// Add the native reader bridge to the explicit viewer representation.
    pub reader: bool,
    /// Strip script blocks from HTML preview representations.
    pub preview: bool,
    /// Sanitized bundle page path supplied by the raw route.
    pub page_path: Option<&'a str>,
    /// Single-artifact title used to derive the attachment filename.
    pub download_title: Option<&'a str>,
    /// Cache/indexing behavior for signed-in raw versus public-share delivery.
    pub cache: RawCachePolicy,
}

/// Match `isHtmlContentType`.
#[must_use]
pub fn is_html_content_type(content_type: &str) -> bool {
    const PREFIX: &str = "text/html";
    content_type
        .get(..PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
        && matches!(content_type.as_bytes().get(PREFIX.len()), None | Some(b';'))
}

/// Name a single-file download from its artifact title.
#[must_use]
pub fn download_name(title: &str) -> String {
    let source = if title.is_empty() { "artifact" } else { title };
    let mut cleaned = String::with_capacity(source.len().min(80));
    let mut pending_dash = false;
    for character in source.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-') {
            if pending_dash && !cleaned.is_empty() {
                cleaned.push('-');
            }
            pending_dash = false;
            cleaned.push(character);
        } else {
            pending_dash = true;
        }
    }
    let cleaned = cleaned.trim_matches('-');
    let base = cleaned.get(..cleaned.len().min(80)).unwrap_or(cleaned);
    let base = if base.is_empty() { "artifact" } else { base };
    format!("{base}.html")
}

/// Inject the server-owned bridge before the last matching `</body>` tag, or append it.
#[must_use]
pub fn inject_anchor_bridge(content: &[u8], page_path: Option<&str>) -> Vec<u8> {
    let html = String::from_utf8_lossy(content);
    let bridge = page_path.map_or_else(
        || ANCHOR_BRIDGE.to_owned(),
        |path| {
            let encoded = serde_json::to_string(path)
                .unwrap_or_else(|_| "null".to_owned())
                .replace('<', "\\u003c");
            let replacement = format!("var d=document,w=window,page={encoded}");
            // Literal replacement: `$` in a page path is data, not a replacement pattern.
            ANCHOR_BRIDGE.replacen(PAGE_INITIALIZER, &replacement, 1)
        },
    );
    let insertion = last_closing_tag(&html, "body").unwrap_or(html.len());
    let mut output = String::with_capacity(html.len() + bridge.len());
    output.push_str(&html[..insertion]);
    output.push_str(&bridge);
    output.push_str(&html[insertion..]);
    output.into_bytes()
}

#[must_use]
pub fn inject_reader_bridge(content: &[u8], _page_path: Option<&str>) -> Vec<u8> {
    let html = String::from_utf8_lossy(content);
    let bridge = format!("<script id=\"{READER_BRIDGE_MARKER}\">{READER_BRIDGE}</script>");
    let insertion = last_closing_tag(&html, "body").unwrap_or(html.len());
    let mut output = String::with_capacity(html.len() + bridge.len());
    output.push_str(&html[..insertion]);
    output.push_str(&bridge);
    output.push_str(&html[insertion..]);
    output.into_bytes()
}

/// Install the data client before published scripts.
#[must_use]
pub fn inject_data_client(content: &[u8]) -> Vec<u8> {
    let html = String::from_utf8_lossy(content);
    let sdk = format!("<script id=\"artifact-data-client\">{DATA_CLIENT}</script>");
    let lower = html.to_ascii_lowercase();
    let insertion = lower
        .match_indices("<script")
        .find(|(offset, _)| {
            lower
                .as_bytes()
                .get(offset + 7)
                .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
        })
        .map_or(0, |(offset, _)| offset);
    format!("{}{}{}", &html[..insertion], sdk, &html[insertion..]).into_bytes()
}

/// Remove closed `<script>...</script>` blocks from the preview representation.
///
/// This is console-noise suppression, not a security boundary. Preview iframes remain sandboxed.
#[must_use]
pub fn strip_scripts(content: &[u8]) -> Vec<u8> {
    let html = String::from_utf8_lossy(content);
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0;
    while let Some((start, end)) = next_script_block(&html, cursor) {
        output.push_str(&html[cursor..start]);
        cursor = end;
    }
    output.push_str(&html[cursor..]);
    output.into_bytes()
}

/// Exact header map returned by `rawArtifactHeaders` before listener middleware adds
/// `no-transform`.
///
/// # Errors
/// Returns [`AppError::Internal`] only if a caller supplies an invalid header
/// value as its content type.
pub fn raw_artifact_headers(
    content_type: &str,
    attachment_name: Option<&str>,
) -> Result<HeaderMap, AppError> {
    let mut headers = HeaderMap::new();
    let content_type = HeaderValue::from_str(content_type)
        .map_err(|error| crate::error::internal("encode content type header", error))?;
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(PRIVATE_CACHE_CONTROL),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DOCUMENT_SANDBOX),
    );
    if let Some(name) = attachment_name {
        let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .map_err(|error| crate::error::internal("encode content disposition header", error))?;
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    Ok(headers)
}

/// Build a raw response from bytes and a MIME type already resolved by the route.
///
/// # Errors
/// Returns [`AppError::Internal`] only if the supplied [`ArtifactFile::content_type`] is not a
/// legal HTTP header value. The artifact store emits only its frozen static MIME table.
pub fn artifact_response(
    file: ArtifactFile,
    options: ArtifactResponseOptions<'_>,
) -> Result<Response<Body>, AppError> {
    let html = is_html_content_type(&file.content_type);
    let attachment_name = options.download_title.map(download_name);
    let mut content = file.content;
    if options.anchor && attachment_name.is_none() && html {
        content = inject_data_client(&inject_anchor_bridge(&content, options.page_path));
    }
    if options.reader && attachment_name.is_none() && html {
        content = inject_reader_bridge(&content, options.page_path);
    }
    if options.preview && html {
        content = strip_scripts(&content);
    }

    let mut headers = raw_artifact_headers(&file.content_type, attachment_name.as_deref())?;
    if options.cache == RawCachePolicy::PublicShare {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(PUBLIC_SHARE_CACHE_CONTROL),
        );
        headers.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    }
    let mut response = Response::new(Body::from(content));
    *response.headers_mut() = headers;
    Ok(response)
}

fn last_closing_tag(html: &str, tag: &str) -> Option<usize> {
    let bytes = html.as_bytes();
    let needle = format!("</{tag}");
    let mut from = 0;
    let mut last = None;
    while let Some(start) = find_ascii_case_insensitive(bytes, needle.as_bytes(), from) {
        let mut cursor = start + needle.len();
        cursor = skip_js_whitespace(html, cursor);
        if bytes.get(cursor) == Some(&b'>') {
            last = Some(start);
        }
        from = start + 1;
    }
    last
}

fn next_script_block(html: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = html.as_bytes();
    let mut search = from;
    while let Some(start) = find_ascii_case_insensitive(bytes, b"<script", search) {
        let after_name = start + b"<script".len();
        let boundary = html[after_name..]
            .chars()
            .next()
            .is_none_or(|character| !(character.is_ascii_alphanumeric() || character == '_'));
        if !boundary {
            search = start + 1;
            continue;
        }
        let open_end = bytes[after_name..]
            .iter()
            .position(|byte| *byte == b'>')
            .map(|offset| after_name + offset + 1)?;
        let mut close_search = open_end;
        while let Some(close) = find_ascii_case_insensitive(bytes, b"</script", close_search) {
            let cursor = skip_js_whitespace(html, close + b"</script".len());
            if bytes.get(cursor) == Some(&b'>') {
                return Some((start, cursor + 1));
            }
            close_search = close + 1;
        }
        return None;
    }
    None
}

fn skip_js_whitespace(value: &str, mut cursor: usize) -> usize {
    while let Some(character) = value[cursor..].chars().next() {
        if !is_js_whitespace(character) {
            break;
        }
        cursor += character.len_utf8();
    }
    cursor
}

fn is_js_whitespace(character: char) -> bool {
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
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|offset| from + offset)
}

#[cfg(test)]
mod anchor_asset_bytes_test {
    #[test]
    fn anchor_bridge_preserves_original_length() {
        assert_eq!(super::ANCHOR_BRIDGE.len(), 13187);
    }
}
