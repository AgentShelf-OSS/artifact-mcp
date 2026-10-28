//! MCP tool handlers grouped by domain. `dispatch::call_tool` routes each tool name here.

mod artifacts;
mod categories;
mod data;
mod feedback;
mod publish;
mod shares;
mod update;

pub(crate) use artifacts::{
    artifact_stats, delete_artifact, list_artifacts, list_revisions, read_artifact,
    restore_artifact, set_visibility,
};
pub(crate) use categories::{create_category, delete_category, list_categories, set_category};
pub(crate) use data::{
    append_artifact_events, data_bindings, list_data_sources, set_artifact_data,
    set_artifact_reminder,
};
pub(crate) use feedback::{list_feedback, reopen_feedback, resolve_feedback, submit_feedback};
pub use publish::{ARTIFACT_KEY_INVALID, is_valid_artifact_key};
pub(crate) use publish::{publish_artifact, publish_bundle};
pub(crate) use shares::{create_share, list_shares, revoke_share};
pub use update::DISCARD_NEWER_INVALID;
pub(crate) use update::{patch_artifact, update_artifact};
