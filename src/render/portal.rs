//! Askama-backed gallery, viewer shell, and standalone pages.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use askama::{Template, filters::HtmlSafe};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::{Position, Url};

use crate::{
    config::AppConfig,
    error::AppError,
    model::{ArtifactMeta, OrgId},
    ports::PageRenderer,
    render::view_models::{GalleryView, SettingsView, ShellView},
    security::access::AccessPolicy,
    security::{
        audit::parse_hmac_key,
        identity::{viewer_display_name, viewer_handle},
    },
};

mod gallery;
mod helpers;
mod renderer;
mod shell;
mod templates;

#[cfg(test)]
mod tests;

use gallery::*;
use helpers::*;
use renderer::*;
use shell::*;
use templates::*;

pub(super) use helpers::org_color;
pub use renderer::{AskamaPageRenderer, TrustedStatic};
pub(super) use renderer::{FAVICON, PORTAL_CSS, THEME_BOOT};
pub use shell::{artifact_install_name, artifact_install_short_name, js_literal};
