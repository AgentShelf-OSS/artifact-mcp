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
use shell::*;
use templates::*;

pub use renderer::AskamaPageRenderer;
pub use shell::{artifact_install_name, artifact_install_short_name, js_literal};

/// A checked-in asset that is allowed to bypass Askama's HTML escaping.
///
/// This wrapper only accepts `&'static str`, which keeps request/database values out of the
/// trusted channel by construction. Runtime values remain ordinary template fields and are
/// always autoescaped.
#[derive(Clone, Copy)]
pub struct TrustedStatic(&'static str);

impl TrustedStatic {
    pub(super) const fn new(value: &'static str) -> Self {
        Self(value)
    }
}

impl fmt::Display for TrustedStatic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl HtmlSafe for TrustedStatic {}

const PORTAL_FAVICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'%3E%3Crect width='64' height='64' rx='7' fill='%23142235'/%3E%3Cpath d='M18 47 30 15h5l12 32h-7l-3-9H27l-3 9Zm11-15h6l-3-10Z' fill='%23D5A252'/%3E%3C/svg%3E";
pub(super) const FAVICON: TrustedStatic = TrustedStatic::new(PORTAL_FAVICON);
pub(super) const PORTAL_CSS: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/portal.css"));
/// The gallery script is one closure split into the parts listed in assets/portal/parts.txt.
/// Keep this list in the same order; a unit test checks the assembled script.
const PORTAL_SCRIPT: TrustedStatic = TrustedStatic::new(concat!(
    include_str!("../../assets/portal/00-prelude-notifications.js"),
    include_str!("../../assets/portal/01-theme-cards.js"),
    include_str!("../../assets/portal/02-filters-sort.js"),
    include_str!("../../assets/portal/03-library-state.js"),
    include_str!("../../assets/portal/04-category-dialog.js"),
    include_str!("../../assets/portal/05-menus.js"),
    include_str!("../../assets/portal/06-share-delete.js"),
    include_str!("../../assets/portal/07-card-actions.js"),
    include_str!("../../assets/portal/08-initialization.js"),
));
const COLLECTION_TOOLBAR_CSS: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/collection-toolbar.css"));
const COLLECTION_TOOLBAR_SCRIPT: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/collection-toolbar.js"));
const COLLECTION_CSS: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/collections.css"));
const COLLECTION_SCRIPT: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/collections.js"));
pub(super) const THEME_BOOT: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/theme-boot.js"));
const NOT_FOUND_CSS: TrustedStatic = TrustedStatic::new(include_str!("../../assets/not-found.css"));
const NOT_SIGNED_IN_CSS: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/not-signed-in.css"));
const ACCESS_RETRY_CSS: TrustedStatic =
    TrustedStatic::new(include_str!("../../assets/access-retry.css"));

pub(super) fn org_color(name: &str, color: Option<&str>) -> String {
    if let Some(color) = color.filter(|color| valid_hex_color(color)) {
        return color.to_owned();
    }
    if name == "admin" {
        return "#66578B".to_owned();
    }
    let mut hash = 2_166_136_261_u32;
    for character in name.chars() {
        let mut units = [0_u16; 2];
        let first = character.encode_utf16(&mut units)[0];
        hash ^= u32::from(first);
        hash = hash.wrapping_mul(16_777_619);
    }
    format!("hsl({} 68% 52%)", hash % 360)
}
