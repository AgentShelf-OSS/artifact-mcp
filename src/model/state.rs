use serde::{Deserialize, Serialize};

use crate::{
    error::AppError,
    model::{ArtifactId, EmailAddress, Timestamp, json::OrderedJson},
};

#[derive(Clone, Debug, PartialEq)]
pub struct StateValue {
    pub artifact_id: ArtifactId,
    pub key: String,
    pub value: OrderedJson,
    pub revision: u64,
    pub updated_at: Timestamp,
    pub updated_by: EmailAddress,
    pub scope: StateScope,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateScope {
    #[default]
    Org,
    Viewer,
}

impl StateScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Org => "org",
            Self::Viewer => "viewer",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateKey {
    pub key: String,
    pub revision: u64,
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub value: OrderedJson,
    pub revision: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{0}")]
    App(#[from] AppError),
    #[error("state conflict")]
    Conflict(Conflict),
    #[error("too many state keys")]
    TooManyKeys,
}
