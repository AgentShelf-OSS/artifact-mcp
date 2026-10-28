//! Identifier alphabets, lengths, and validators.

/// Artifact id alphabet — `customAlphabet("0123456789abcdefghijkmnpqrstuvwxyz", 12)`.
/// Note the deliberately absent `l` and `o`. [lib/store.js:30]
pub const ARTIFACT_ID_ALPHABET: &str = "0123456789abcdefghijkmnpqrstuvwxyz";
/// Artifact id length — [lib/store.js:30]
pub const ARTIFACT_ID_LENGTH: usize = 12;

/// Public share token alphabet (nanoid's URL-safe alphabet) — [lib/shares.js:6]
pub const SHARE_TOKEN_ALPHABET: &str =
    "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_-";
/// Public share token length — [lib/shares.js:6]
pub const SHARE_TOKEN_LENGTH: usize = 24;

/// Feedback id alphabet — `customAlphabet("0123456789abcdefghijklmnopqrstuvwxyz", 16)`.
/// Unlike artifact ids this one keeps `l` and `o`. [lib/feedback.js:10]
pub const FEEDBACK_ID_ALPHABET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
/// Feedback id length — [lib/feedback.js:10]
pub const FEEDBACK_ID_LENGTH: usize = 16;

/// Webhook id alphabet — shares the artifact alphabet. [lib/webhooks.js:11]
pub const WEBHOOK_ID_ALPHABET: &str = ARTIFACT_ID_ALPHABET;
/// Webhook id length — [lib/webhooks.js:11]
pub const WEBHOOK_ID_LENGTH: usize = 12;

/// Whether a value has the exact persisted webhook-row identifier shape.
///
/// Audit records may retain this server-generated path identifier, but never the webhook URL or
/// another arbitrary identifier-shaped value. Keeping this validator beside the generator avoids
/// the two runtimes drifting on alphabet or length.
#[must_use]
pub fn is_valid_webhook_id(value: &str) -> bool {
    value.len() == WEBHOOK_ID_LENGTH
        && value
            .bytes()
            .all(|byte| WEBHOOK_ID_ALPHABET.as_bytes().contains(&byte))
}

/// `const ARTIFACT_ID = /^[0-9a-z]{6,24}$/` — the real validator every generated artifact
/// id must satisfy before a thumbnail path is derived from it. [lib/thumbnails.js:19]
#[must_use]
pub fn is_valid_artifact_id(value: &str) -> bool {
    (6..=24).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
}

/// `const RESERVED = new Set([...])` — ids that can never address an artifact.
/// The empty string is the seventh member in Node and is excluded here by construction.
/// [lib/store.js:29]
pub const RESERVED_ARTIFACT_IDS: [&str; 10] = [
    "mcp",
    "health",
    "settings",
    "raw",
    "s",
    "favicon.ico",
    "robots.txt",
    // ADR-0012 static Web Push files.
    "sw.js",
    "manifest.webmanifest",
    "icons",
];
