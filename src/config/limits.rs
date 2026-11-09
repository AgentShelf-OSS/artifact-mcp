//! Storage and per-route JSON body limits.

use crate::error::AppError;

use super::{
    defaults::{
        DEFAULT_CATEGORY_JSON_LIMIT, DEFAULT_FEEDBACK_JSON_LIMIT, DEFAULT_FEEDBACK_MAX_BODY,
        DEFAULT_KEY_JSON_LIMIT, DEFAULT_MAX_ARTIFACT_BYTES, DEFAULT_MAX_BUNDLE_BYTES,
        DEFAULT_MAX_BUNDLE_FILES, DEFAULT_MAX_HISTORY, DEFAULT_REACTION_JSON_LIMIT,
        DEFAULT_STATE_JSON_LIMIT, mcp_json_limit_for,
    },
    env::EnvSource,
    parse::{parse_byte_size, positive_integer, present},
};

/// Upload/storage limits shared by the store and the MCP tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageLimits {
    /// `FEEDBACK_MAX_BODY` — maximum feedback body characters.
    pub feedback_max_body: u64,
    /// `MAX_HISTORY` — retained revisions per artifact.
    pub max_history: u64,
    /// `MAX_ARTIFACT_BYTES` — maximum single-file artifact size.
    pub max_artifact_bytes: u64,
    /// `MAX_BUNDLE_BYTES` — maximum total bundle size.
    pub max_bundle_bytes: u64,
    /// `MAX_BUNDLE_FILES` — maximum entries in a bundle.
    pub max_bundle_files: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            feedback_max_body: DEFAULT_FEEDBACK_MAX_BODY,
            max_history: DEFAULT_MAX_HISTORY,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            max_bundle_bytes: DEFAULT_MAX_BUNDLE_BYTES,
            max_bundle_files: DEFAULT_MAX_BUNDLE_FILES,
        }
    }
}

impl StorageLimits {
    pub(super) fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        Ok(Self {
            feedback_max_body: positive_integer(
                env,
                "FEEDBACK_MAX_BODY",
                DEFAULT_FEEDBACK_MAX_BODY,
            )?,
            max_history: positive_integer(env, "MAX_HISTORY", DEFAULT_MAX_HISTORY)?,
            max_artifact_bytes: positive_integer(
                env,
                "MAX_ARTIFACT_BYTES",
                DEFAULT_MAX_ARTIFACT_BYTES,
            )?,
            max_bundle_bytes: positive_integer(env, "MAX_BUNDLE_BYTES", DEFAULT_MAX_BUNDLE_BYTES)?,
            max_bundle_files: positive_integer(env, "MAX_BUNDLE_FILES", DEFAULT_MAX_BUNDLE_FILES)?,
        })
    }
}

/// Per-route JSON body limits in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyLimits {
    /// `POST /mcp`.
    pub mcp_json: u64,
    /// `/settings/keys`, `/settings/orgs/**`
    pub key_json: u64,
    /// `POST /:id/react`
    pub reaction_json: u64,
    /// `POST /:id/feedback`
    pub feedback_json: u64,
    /// `PUT /:id/state/:key` — serialized value cap plus envelope overhead.
    pub state_json: u64,
    /// `POST /:id/{category,share,visibility,move,restore}`
    pub category_json: u64,
}

impl Default for BodyLimits {
    fn default() -> Self {
        Self {
            mcp_json: mcp_json_limit_for(DEFAULT_MAX_BUNDLE_BYTES),
            key_json: DEFAULT_KEY_JSON_LIMIT,
            reaction_json: DEFAULT_REACTION_JSON_LIMIT,
            feedback_json: DEFAULT_FEEDBACK_JSON_LIMIT,
            state_json: DEFAULT_STATE_JSON_LIMIT,
            category_json: DEFAULT_CATEGORY_JSON_LIMIT,
        }
    }
}

impl BodyLimits {
    pub(super) fn from_source(
        env: &dyn EnvSource,
        max_bundle_bytes: u64,
    ) -> Result<Self, AppError> {
        // `MCP_JSON_LIMIT` accepts a raw size string, so it is the only limit parsed
        // with the historical `bytes` grammar.
        let mcp_json = match present(env, "MCP_JSON_LIMIT") {
            Some(raw) => parse_byte_size("MCP_JSON_LIMIT", &raw)?,
            None => mcp_json_limit_for(max_bundle_bytes),
        };
        Ok(Self {
            mcp_json,
            ..Self::default()
        })
    }
}
