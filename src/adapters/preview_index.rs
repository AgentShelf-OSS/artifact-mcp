//! Startup-time artifact index used by the preview cache audit.

use std::collections::HashMap;

use crate::{
    artifacts::validation::SafeArtifactId,
    integrations::thumbnails::{PreviewArtifactIndex, PreviewArtifactRef},
    model::ArtifactMeta,
};

#[derive(Default)]
pub struct StartupPreviewIndex {
    artifacts: HashMap<String, PreviewArtifactRef>,
}

impl PreviewArtifactIndex for StartupPreviewIndex {
    fn artifact(&self, id: &SafeArtifactId) -> Option<PreviewArtifactRef> {
        self.artifacts.get(id.as_str()).cloned()
    }
}

impl StartupPreviewIndex {
    /// Index the startup artifact set by id for the preview cache audit.
    pub fn from_artifacts(artifacts: &[ArtifactMeta]) -> Self {
        Self {
            artifacts: artifacts
                .iter()
                .map(|meta| {
                    (
                        meta.id.0.clone(),
                        PreviewArtifactRef {
                            is_bundle: meta.is_bundle,
                            body_sha256: meta.body_sha256.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}
