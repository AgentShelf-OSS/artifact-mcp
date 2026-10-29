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
const PORTAL_SCRIPT: TrustedStatic = TrustedStatic::new(include_str!("../../assets/portal.js"));
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

type RenderClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Production renderer for every page behind the frozen [`PageRenderer`] port.
#[derive(Clone)]
pub struct AskamaPageRenderer {
    app_name: String,
    app_brand: String,
    site_host: String,
    clock: RenderClock,
    audit_key: Option<[u8; 32]>,
}

impl AskamaPageRenderer {
    #[must_use]
    pub fn from_config(config: &AppConfig) -> Self {
        let mut renderer = Self::new(
            config.app_name.clone(),
            config.app_brand.clone(),
            &config.public_base_url,
        );
        renderer.audit_key = config
            .audit_ledger_hmac_key
            .as_ref()
            .and_then(|v| parse_hmac_key(v.expose()).ok());
        renderer
    }

    #[must_use]
    pub fn new(app_name: String, app_brand: String, public_base_url: &str) -> Self {
        Self {
            app_name,
            app_brand,
            site_host: site_host(public_base_url),
            clock: Arc::new(system_now_unix_seconds),
            audit_key: None,
        }
    }

    /// Deterministic rendering seam for fixed-clock snapshots.
    #[must_use]
    pub fn with_fixed_clock(config: &AppConfig, unix_seconds: i64) -> Self {
        let mut renderer = Self::from_config(config);
        renderer.clock = Arc::new(move || unix_seconds);
        renderer
    }
}

impl Default for AskamaPageRenderer {
    fn default() -> Self {
        Self::from_config(&AppConfig::default())
    }
}

fn system_now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

fn site_host(public_base_url: &str) -> String {
    Url::parse(public_base_url)
        .ok()
        .map(|url| url[Position::BeforeHost..Position::AfterPort].to_owned())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "localhost:3480".to_owned())
}

#[derive(Clone, Copy)]
struct Icons {
    search: TrustedStatic,
    bell: TrustedStatic,
    theme: TrustedStatic,
    signout: TrustedStatic,
    open: TrustedStatic,
    download: TrustedStatic,
    heart: TrustedStatic,
    up: TrustedStatic,
    down: TrustedStatic,
    back: TrustedStatic,
    forward: TrustedStatic,
    eye: TrustedStatic,
    eye_off: TrustedStatic,
    comment: TrustedStatic,
    share: TrustedStatic,
    more: TrustedStatic,
}

const ICONS: Icons = Icons {
    search: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="11" cy="11" r="7"></circle><path d="m20 20-3.8-3.8"></path></svg>"#,
    ),
    bell: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9"></path><path d="M10 21h4"></path></svg>"#,
    ),
    theme: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20.5 15.2A8.5 8.5 0 0 1 8.8 3.5 8.5 8.5 0 1 0 20.5 15.2Z"></path></svg>"#,
    ),
    signout: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M10 5H5v14h5"></path><path d="m14 8 4 4-4 4M8 12h10"></path></svg>"#,
    ),
    open: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14 5h5v5M19 5l-8 8"></path><path d="M19 13v6H5V5h6"></path></svg>"#,
    ),
    download: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4v11m0 0 4-4m-4 4-4-4"></path><path d="M5 19h14"></path></svg>"#,
    ),
    heart: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.7l-1.1-1.1a5.5 5.5 0 0 0-7.8 7.8l1.1 1.1L12 21l7.8-7.5 1.1-1.1a5.5 5.5 0 0 0-.1-7.8Z"></path></svg>"#,
    ),
    up: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 11 5-6 5 6"></path><path d="M12 5v14"></path></svg>"#,
    ),
    down: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m7 13 5 6 5-6"></path><path d="M12 5v14"></path></svg>"#,
    ),
    back: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m15 18-6-6 6-6"></path></svg>"#,
    ),
    forward: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 18 6-6-6-6"></path></svg>"#,
    ),
    eye: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"></path><circle cx="12" cy="12" r="2.5"></circle></svg>"#,
    ),
    eye_off: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m3 3 18 18"></path><path d="M10.6 6.2A10.5 10.5 0 0 1 12 6c6 0 9.5 6 9.5 6a17.7 17.7 0 0 1-3.1 3.8M6.1 6.1C3.8 7.7 2.5 10 2.5 12c0 0 3.5 6 9.5 6 1.4 0 2.7-.3 3.8-.8"></path><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2"></path></svg>"#,
    ),
    comment: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M21 15a4 4 0 0 1-4 4H8l-5 3V7a4 4 0 0 1 4-4h10a4 4 0 0 1 4 4Z"></path><path d="M8 9h8M8 13h5"></path></svg>"#,
    ),
    share: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="18" cy="5" r="3"></circle><circle cx="6" cy="12" r="3"></circle><circle cx="18" cy="19" r="3"></circle><path d="m8.7 10.6 6.6-4.1M8.7 13.4l6.6 4.1"></path></svg>"#,
    ),
    more: TrustedStatic::new(
        r#"<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="5" cy="12" r="1"></circle><circle cx="12" cy="12" r="1"></circle><circle cx="19" cy="12" r="1"></circle></svg>"#,
    ),
};

#[derive(Template)]
#[template(path = "gallery.html")]
struct GalleryTemplate<'a> {
    collection_toolbar_css: TrustedStatic,
    collection_toolbar_script: TrustedStatic,
    collection_css: TrustedStatic,
    collection_script: TrustedStatic,
    favicon: TrustedStatic,
    theme_boot: TrustedStatic,
    css: TrustedStatic,
    script: TrustedStatic,
    icons: Icons,
    app_name: &'a str,
    app_brand: &'a str,
    site_host: &'a str,
    viewer_email: String,
    viewer_is_admin: bool,
    viewer_org: String,
    role: String,
    identity_color: String,
    total: usize,
    organization_total: usize,
    favorite_total: usize,
    needs_review_total: usize,
    hidden_total: usize,
    has_delete_actions: bool,
    cards: Vec<CardTemplate>,
    chips: Vec<GalleryChipTemplate>,
    categories: Vec<CategoryFilterTemplate>,
    org_category_json: String,
    notifications: Vec<NotificationTemplate>,
    unread_notifications: u64,
}

struct GalleryChipTemplate {
    org: String,
    count: usize,
}

struct NotificationTemplate {
    href: String,
    unread: bool,
    viewer_email: String,
    relative_time: String,
    artifact_title: String,
    snippet: String,
}

struct CategoryFilterTemplate {
    key: String,
    label: String,
    count: usize,
}

#[derive(Clone, Serialize)]
struct OrgCategoryTemplate {
    name: String,
    count: usize,
}

struct CardTemplate {
    id: String,
    org: String,
    title: String,
    description: String,
    has_description: bool,
    who: String,
    is_bundle: bool,
    hidden: bool,
    favorite: bool,
    vote: i8,
    needs_review: bool,
    is_owned_by_viewer: bool,
    show_visibility: bool,
    show_delete: bool,
    category: String,
    has_category: bool,
    category_label: String,
    color: String,
    thumbnail_src: String,
    search_text: String,
    updated_datetime: String,
    updated_label: String,
    bytes_label: String,
    show_admin: bool,
    sentiment_favorites: u64,
    sentiment_up: u64,
    sentiment_down: u64,
    has_views: bool,
    views: u64,
    unique_viewers: u64,
    unique_viewers_plural: bool,
    category_options: Vec<MoveOptionTemplate>,
    org_options: Vec<MoveOptionTemplate>,
}

struct MoveOptionTemplate {
    value: String,
    label: String,
    selected: bool,
}

const SHELL_CSS: TrustedStatic = TrustedStatic::new(concat!(
    include_str!("../../assets/dialogs.css"),
    "\n",
    include_str!("../../assets/shell.css")
));
const SHELL_SCRIPT: TrustedStatic = TrustedStatic::new(concat!(
    include_str!("../../assets/dialogs.js"),
    "\n",
    include_str!("../../assets/shell.js")
));
const ACTION_SCRIPT: TrustedStatic = TrustedStatic::new(include_str!("../../assets/actions.js"));
const CAST_SCRIPT: TrustedStatic = TrustedStatic::new(include_str!("../../assets/cast.js"));

#[derive(Template)]
#[template(path = "artifact-shell.html")]
struct ShellTemplate<'a> {
    viewer_boot: TrustedStatic,
    favicon: TrustedStatic,
    theme_boot: TrustedStatic,
    css: TrustedStatic,
    script: TrustedStatic,
    cast_script: TrustedStatic,
    action_script: TrustedStatic,
    cast_enabled: bool,
    icons: Icons,
    app_name: &'a str,
    title: String,
    org: String,
    color: String,
    who: String,
    is_bundle: bool,
    category: String,
    has_category: bool,
    favorite: bool,
    vote: i8,
    previous_id: String,
    has_previous: bool,
    next_id: String,
    has_next: bool,
    navigation_index: usize,
    navigation_total: usize,
    views: u64,
    has_viewers: bool,
    unresolved: usize,
    revision: u64,
    raw_src: String,
    anchor_raw_src: String,
    threads: Vec<FeedbackThreadTemplate>,
    viewers: Vec<ViewerTemplate>,
    artifact_id: String,
    /// Home Screen title; see [`artifact_install_name`].
    install_name: String,
    artifact_id_literal: String,
    previous_id_literal: String,
    next_id_literal: String,
    bundle_raw_prefix_literal: String,
    version_query_literal: String,
    viewer_email_literal: String,
    viewer_email: String,
    viewer_id_literal: String,
    viewer_name_literal: String,
    viewer_is_admin: bool,
    state_enabled: bool,
    can_delete: bool,
    feedback_literal: String,
    title_literal: String,
    bytes: u64,
}

struct FeedbackThreadTemplate {
    parent: FeedbackTemplate,
    replies: Vec<FeedbackTemplate>,
}

struct FeedbackTemplate {
    id: String,
    viewer_email: String,
    body: String,
    created_at: String,
    resolved: bool,
    manageable: bool,
    anchor_state: String,
    has_anchor_state: bool,
}

struct ViewerTemplate {
    email: String,
    count: u64,
    count_plural: bool,
    last_seen: String,
}

#[derive(Serialize)]
struct FeedbackScriptRow<'a> {
    id: &'a str,
    parent_id: Option<&'a str>,
    viewer_email: &'a str,
    author_source: Option<&'a str>,
    external_author_display: Option<&'a str>,
    body: &'a str,
    created_at: &'a str,
    resolved_at: Option<&'a str>,
    anchor_path: Option<&'a str>,
    anchor_x: Option<f64>,
    anchor_y: Option<f64>,
    anchor_w: Option<f64>,
    anchor_h: Option<f64>,
    anchor_approx: bool,
    anchor_page: Option<&'a str>,
    anchor_page_stale: bool,
    anchor_kind: Option<&'a str>,
    anchor_node_id: Option<&'a str>,
    anchor_quote: Option<&'a str>,
    anchor_version: u8,
    artifact_revision: u64,
}

/// Fallback install name when an artifact title has no printable text.
const INSTALL_NAME_FALLBACK: &str = "Artifact";

/// Remove control characters, trim, keep at most `limit` code points, and trim the end again.
///
/// Mirrors Node's `installName` in `lib/portal.js`: `char::is_control` is the Unicode `Cc`
/// category (U+0000-U+001F and U+007F-U+009F) and `char::is_whitespace` is `White_Space`,
/// matching `\p{White_Space}`. Counting `char`s never splits a code point.
fn install_name(value: &str, limit: usize) -> String {
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    let cut: String = clean.trim().chars().take(limit).collect();
    let cut = cut.trim_end();
    if cut.is_empty() {
        INSTALL_NAME_FALLBACK.to_owned()
    } else {
        cut.to_owned()
    }
}

/// Installed-app name for one artifact: at most 60 code points, fallback `Artifact`.
#[must_use]
pub fn artifact_install_name(title: &str) -> String {
    install_name(title, 60)
}

/// Installed-app short name for one artifact: at most 12 code points, fallback `Artifact`.
#[must_use]
pub fn artifact_install_short_name(title: &str) -> String {
    install_name(&artifact_install_name(title), 12)
}

/// Encode one JavaScript string literal for an inline-script boundary.
///
/// All runtime strings entering shell JavaScript pass through this function exactly once. The
/// result is placed in an autoescaped `data-*` attribute, then parsed by the checked-in script;
/// no request or database value is marked HTML-safe.
#[must_use]
pub fn js_literal(value: &str) -> String {
    let encoded = match serde_json::to_string(value) {
        Ok(encoded) => encoded,
        Err(_) => "\"\"".to_owned(),
    };
    encoded
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn shell_template<'a>(
    renderer: &'a AskamaPageRenderer,
    view: &ShellView,
) -> Result<ShellTemplate<'a>, AppError> {
    let meta = view.artifact.meta();
    let viewer_email = view
        .viewer
        .email
        .as_ref()
        .map_or("", |email| email.0.as_str());
    let (viewer_id, viewer_name) = shell_viewer_identity(renderer, view, viewer_email);
    let who = if meta.uploader_label.is_empty() {
        meta.client_id.0.clone()
    } else {
        meta.uploader_label.clone()
    };
    let threads = shell_feedback_threads(view, meta.revision, viewer_email);
    let feedback_json = shell_feedback_json(view)?;
    let raw_src = if meta.is_bundle {
        format!("/raw/{}/", meta.id.0)
    } else {
        format!("/raw/{}", meta.id.0)
    };
    let version = body_version(meta);
    let version_query = if version.is_empty() {
        String::new()
    } else {
        format!("&v={version}")
    };
    let (previous_id, next_id) = shell_navigation(view);
    let viewers = shell_viewers(view);
    Ok(ShellTemplate {
        viewer_boot: TrustedStatic::new(include_str!("../../assets/viewer-boot.js")),
        favicon: FAVICON,
        theme_boot: THEME_BOOT,
        css: SHELL_CSS,
        script: SHELL_SCRIPT,
        cast_script: CAST_SCRIPT,
        action_script: ACTION_SCRIPT,
        cast_enabled: view.cast_enabled,
        icons: ICONS,
        app_name: &renderer.app_name,
        title: meta.title.clone(),
        org: meta.org.0.clone(),
        color: org_color(&meta.org.0, view.org_accent.as_deref()),
        who,
        is_bundle: meta.is_bundle,
        category: meta.category.clone(),
        has_category: !meta.category.is_empty(),
        favorite: view.reaction.favorite != 0,
        vote: view.reaction.vote,
        previous_id: previous_id.clone(),
        has_previous: !previous_id.is_empty(),
        next_id: next_id.clone(),
        has_next: !next_id.is_empty(),
        navigation_index: view.navigation.index,
        navigation_total: view.navigation.total,
        views: view.view_counts.views,
        has_viewers: view.viewers.is_some(),
        unresolved: view
            .feedback
            .iter()
            .filter(|feedback| feedback.resolved_at.is_none())
            .count(),
        revision: meta.revision,
        raw_src: raw_src.clone(),
        anchor_raw_src: if view.cast_enabled && !meta.is_bundle {
            format!(
                "{raw_src}?anchor=1&reader=1&cast-pin={}.{}{version_query}",
                meta.revision, meta.body_sha256
            )
        } else {
            format!("{raw_src}?anchor=1&reader=1{version_query}")
        },
        threads,
        viewers,
        artifact_id: meta.id.0.clone(),
        install_name: artifact_install_name(&meta.title),
        artifact_id_literal: js_literal(&meta.id.0),
        previous_id_literal: js_literal(&previous_id),
        next_id_literal: js_literal(&next_id),
        bundle_raw_prefix_literal: js_literal(&format!("/raw/{}/", meta.id.0)),
        version_query_literal: js_literal(&version_query),
        viewer_email_literal: js_literal(viewer_email),
        viewer_email: viewer_email.to_owned(),
        viewer_id_literal: js_literal(&viewer_id),
        viewer_name_literal: js_literal(&viewer_name),
        viewer_is_admin: view.viewer.is_admin,
        state_enabled: !viewer_email.is_empty(),
        can_delete: AccessPolicy::viewer_can_manage_artifact(&view.viewer, meta),
        feedback_literal: js_literal(&feedback_json),
        title_literal: js_literal(&meta.title),
        bytes: meta.bytes,
    })
}

fn shell_viewer_identity(
    renderer: &AskamaPageRenderer,
    view: &ShellView,
    viewer_email: &str,
) -> (String, String) {
    let viewer_id = renderer.audit_key.map_or_else(String::new, |key| {
        if viewer_email.is_empty() {
            String::new()
        } else {
            viewer_handle(viewer_email, &key)
        }
    });
    let viewer_name = view.viewer_display_name.clone().unwrap_or_else(|| {
        if viewer_email.is_empty() {
            String::new()
        } else {
            viewer_display_name(viewer_email)
        }
    });
    (viewer_id, viewer_name)
}

fn shell_feedback_threads(
    view: &ShellView,
    revision: u64,
    viewer_email: &str,
) -> Vec<FeedbackThreadTemplate> {
    view.feedback
        .iter()
        .filter(|feedback| feedback.parent_id.is_none())
        .map(|parent| FeedbackThreadTemplate {
            parent: feedback_template(parent, revision, viewer_email, view.viewer.is_admin),
            replies: view
                .feedback
                .iter()
                .filter(|reply| reply.parent_id.as_ref() == Some(&parent.id))
                .map(|reply| feedback_template(reply, revision, viewer_email, view.viewer.is_admin))
                .collect(),
        })
        .collect()
}

fn shell_feedback_json(view: &ShellView) -> Result<String, AppError> {
    serde_json::to_string(
        &view
            .feedback
            .iter()
            .map(|feedback| {
                let (author_source, external_author_display) = match &feedback.author {
                    crate::model::FeedbackAuthor::Artifact { .. } => (None, None),
                    crate::model::FeedbackAuthor::Discord {
                        external_author_display,
                        ..
                    } => (Some("discord"), Some(external_author_display.as_str())),
                };
                FeedbackScriptRow {
                    id: &feedback.id.0,
                    parent_id: feedback.parent_id.as_ref().map(|id| id.0.as_str()),
                    viewer_email: feedback_author_label(&feedback.author),
                    author_source,
                    external_author_display,
                    body: &feedback.body,
                    created_at: &feedback.created_at.0,
                    resolved_at: feedback.resolved_at.as_ref().map(|value| value.0.as_str()),
                    anchor_path: feedback.anchor_path.as_deref(),
                    anchor_x: feedback.anchor_x,
                    anchor_y: feedback.anchor_y,
                    anchor_w: feedback.anchor_w,
                    anchor_h: feedback.anchor_h,
                    anchor_approx: feedback.anchor_approx,
                    anchor_page: feedback.anchor_page.as_deref(),
                    // The frozen Feedback model does not expose Node's projection-only stale flag.
                    anchor_page_stale: false,
                    anchor_kind: feedback.anchor_kind.as_deref(),
                    anchor_node_id: feedback.anchor_node_id.as_deref(),
                    anchor_quote: feedback.anchor_quote.as_deref(),
                    anchor_version: feedback.anchor_version,
                    artifact_revision: feedback.artifact_revision,
                }
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|_| AppError::Internal)
}

fn shell_navigation(view: &ShellView) -> (String, String) {
    let previous_id = view
        .navigation
        .previous_id
        .as_ref()
        .map_or_else(String::new, |id| id.0.clone());
    let next_id = view
        .navigation
        .next_id
        .as_ref()
        .map_or_else(String::new, |id| id.0.clone());
    (previous_id, next_id)
}

fn shell_viewers(view: &ShellView) -> Vec<ViewerTemplate> {
    view.viewers
        .as_ref()
        .into_iter()
        .flatten()
        .map(|viewer| ViewerTemplate {
            email: viewer.email.0.clone(),
            count: viewer.count,
            count_plural: viewer.count != 1,
            last_seen: format_date(&viewer.last_viewed_at.0),
        })
        .collect()
}

fn feedback_author_label(author: &crate::model::FeedbackAuthor) -> &str {
    match author {
        crate::model::FeedbackAuthor::Artifact { viewer_email } => &viewer_email.0,
        crate::model::FeedbackAuthor::Discord {
            external_author_display,
            ..
        } => external_author_display,
    }
}

fn feedback_template(
    feedback: &crate::model::Feedback,
    current_revision: u64,
    viewer_email: &str,
    viewer_is_admin: bool,
) -> FeedbackTemplate {
    let anchored = feedback.anchor_x.is_some() && feedback.anchor_y.is_some();
    let box_anchor = feedback.anchor_w.is_some() && feedback.anchor_h.is_some();
    let anchor_state = if !anchored {
        String::new()
    } else if feedback.artifact_revision != current_revision {
        format!("Placed on v{} · stale", feedback.artifact_revision)
    } else if box_anchor {
        "Pinned section".to_owned()
    } else {
        "Pinned comment".to_owned()
    };
    let (author_label, manageable) = match &feedback.author {
        crate::model::FeedbackAuthor::Artifact {
            viewer_email: author,
        } => (
            author.0.clone(),
            viewer_is_admin || author.0 == viewer_email,
        ),
        crate::model::FeedbackAuthor::Discord {
            external_author_display,
            ..
        } => (
            format!("{external_author_display} · Discord"),
            viewer_is_admin,
        ),
    };
    FeedbackTemplate {
        id: feedback.id.0.clone(),
        viewer_email: author_label,
        body: feedback.body.clone(),
        created_at: format_date(&feedback.created_at.0),
        resolved: feedback.resolved_at.is_some(),
        manageable,
        has_anchor_state: !anchor_state.is_empty(),
        anchor_state,
    }
}

fn gallery_template<'a>(
    renderer: &'a AskamaPageRenderer,
    view: &GalleryView,
) -> GalleryTemplate<'a> {
    let collection = gallery_collection(view);
    let (org_category_index, categories) = gallery_categories(view);
    let now = (renderer.clock)();
    let cards = gallery_cards(&collection, view, &org_category_index, now);
    let has_delete_actions = cards.iter().any(|card| card.show_delete);
    let notifications = gallery_notifications(view, now);
    let (viewer_org, identity_color) = gallery_identity(view, collection.is_admin);
    let org_category_json =
        serde_json::to_string(&org_category_index).unwrap_or_else(|_| "{}".to_owned());
    GalleryTemplate {
        collection_toolbar_css: COLLECTION_TOOLBAR_CSS,
        collection_toolbar_script: COLLECTION_TOOLBAR_SCRIPT,
        collection_css: COLLECTION_CSS,
        collection_script: COLLECTION_SCRIPT,
        favicon: FAVICON,
        theme_boot: THEME_BOOT,
        css: PORTAL_CSS,
        script: PORTAL_SCRIPT,
        icons: ICONS,
        app_name: &renderer.app_name,
        app_brand: &renderer.app_brand,
        site_host: &renderer.site_host,
        viewer_email: view
            .viewer
            .email
            .as_ref()
            .map_or_else(String::new, |email| email.0.clone()),
        viewer_is_admin: collection.is_admin,
        viewer_org: if viewer_org.is_empty() {
            "your organization".to_owned()
        } else {
            viewer_org.clone()
        },
        role: if collection.is_admin {
            "All organizations".to_owned()
        } else if viewer_org.is_empty() {
            "Member".to_owned()
        } else {
            viewer_org.clone()
        },
        identity_color,
        total: collection.artifacts.len(),
        organization_total: collection.organization_total,
        favorite_total: collection.favorite_total,
        needs_review_total: collection.needs_review_total,
        hidden_total: collection.hidden_total,
        has_delete_actions,
        cards,
        chips: collection.chips,
        categories,
        org_category_json,
        notifications,
        unread_notifications: view.unread_notifications,
    }
}

struct GalleryCollection<'a> {
    is_admin: bool,
    org_names: Vec<String>,
    artifacts: Vec<&'a ArtifactMeta>,
    organization_total: usize,
    favorite_total: usize,
    needs_review_total: usize,
    hidden_total: usize,
    chips: Vec<GalleryChipTemplate>,
}

fn gallery_collection(view: &GalleryView) -> GalleryCollection<'_> {
    let is_admin = view.viewer.is_admin;
    let org_names = view
        .sections
        .iter()
        .map(|section| section.org.0.clone())
        .collect();
    let mut artifacts: Vec<&ArtifactMeta> = view
        .sections
        .iter()
        .flat_map(|section| section.items.iter())
        .collect();
    artifacts.sort_by(|left, right| {
        modified(right)
            .encode_utf16()
            .cmp(modified(left).encode_utf16())
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
    let favorite_total = artifacts
        .iter()
        .filter(|artifact| {
            view.reactions
                .get(&artifact.id)
                .is_some_and(|reaction| reaction.favorite != 0)
        })
        .count();
    let needs_review_total = artifacts
        .iter()
        .filter(|artifact| {
            if is_admin {
                view.sentiment
                    .get(&artifact.id)
                    .is_some_and(|sentiment| sentiment.down > 0)
            } else {
                view.reactions
                    .get(&artifact.id)
                    .is_some_and(|reaction| reaction.vote < 0)
            }
        })
        .count();
    let hidden_total = artifacts.iter().filter(|artifact| artifact.hidden).count();
    let chips = view
        .sections
        .iter()
        .map(|section| GalleryChipTemplate {
            org: section.org.0.clone(),
            count: section.items.len(),
        })
        .collect();
    GalleryCollection {
        is_admin,
        org_names,
        artifacts,
        organization_total: view.sections.len(),
        favorite_total,
        needs_review_total,
        hidden_total,
        chips,
    }
}

fn gallery_categories(
    view: &GalleryView,
) -> (
    BTreeMap<String, Vec<OrgCategoryTemplate>>,
    Vec<CategoryFilterTemplate>,
) {
    let mut index = BTreeMap::<String, Vec<OrgCategoryTemplate>>::new();
    for section in &view.sections {
        let mut counts = BTreeMap::new();
        for artifact in &section.items {
            *counts
                .entry(js_trim(&artifact.category).to_owned())
                .or_default() += 1;
        }
        let mut names = view
            .org_categories
            .get(&section.org)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|name| js_trim(&name).to_owned())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        names.extend(counts.keys().cloned());
        names.sort_by(|left, right| category_cmp(left, right));
        names.dedup();
        index.insert(
            section.org.0.clone(),
            names
                .into_iter()
                .map(|name| OrgCategoryTemplate {
                    count: counts.get(&name).copied().unwrap_or(0),
                    name,
                })
                .collect(),
        );
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for category in index.values().flatten() {
        if let Some((_, count)) = counts.iter_mut().find(|(name, _)| name == &category.name) {
            *count += category.count;
        } else {
            counts.push((category.name.clone(), category.count));
        }
    }
    counts.sort_by(|(left, _), (right, _)| category_cmp(left, right));
    let categories = counts
        .into_iter()
        .map(|(key, count)| CategoryFilterTemplate {
            label: if key.is_empty() {
                "Uncategorized".to_owned()
            } else {
                key.clone()
            },
            key,
            count,
        })
        .collect();
    (index, categories)
}

fn gallery_cards(
    collection: &GalleryCollection<'_>,
    view: &GalleryView,
    index: &BTreeMap<String, Vec<OrgCategoryTemplate>>,
    now: i64,
) -> Vec<CardTemplate> {
    collection
        .artifacts
        .iter()
        .map(|artifact| {
            let category_names = index
                .get(&artifact.org.0)
                .map(|categories| {
                    categories
                        .iter()
                        .map(|category| category.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            card_template(
                artifact,
                view,
                &collection.org_names,
                &category_names,
                collection.is_admin,
                now,
            )
        })
        .collect()
}

fn gallery_notifications(view: &GalleryView, now: i64) -> Vec<NotificationTemplate> {
    view.notifications
        .iter()
        .map(|notification| {
            let author = match &notification.author {
                crate::model::FeedbackAuthor::Artifact { viewer_email } => viewer_email.0.clone(),
                crate::model::FeedbackAuthor::Discord {
                    external_author_display,
                    ..
                } => format!("{external_author_display} · Discord"),
            };
            NotificationTemplate {
                href: format!(
                    "/{}?feedback={}",
                    encode_uri_component(&notification.artifact_id.0),
                    encode_uri_component(&notification.id.0)
                ),
                unread: notification.unread,
                viewer_email: author,
                relative_time: relative_time(&notification.created_at.0, now),
                artifact_title: notification.artifact_title.clone(),
                snippet: notification_snippet(&notification.body),
            }
        })
        .collect()
}

fn gallery_identity(view: &GalleryView, is_admin: bool) -> (String, String) {
    let viewer_org = view
        .viewer
        .org
        .as_ref()
        .map_or_else(String::new, |org| org.0.clone());
    let identity_org = if is_admin { "admin" } else { &viewer_org };
    let identity_color = view
        .org_colors
        .get(&OrgId::from(identity_org))
        .and_then(Option::as_deref);
    let identity_color = org_color(identity_org, identity_color);
    (viewer_org, identity_color)
}

fn card_template(
    artifact: &ArtifactMeta,
    view: &GalleryView,
    org_names: &[String],
    category_names: &[String],
    is_admin: bool,
    now: i64,
) -> CardTemplate {
    let reaction = view
        .reactions
        .get(&artifact.id)
        .copied()
        .unwrap_or_default();
    let sentiment = view
        .sentiment
        .get(&artifact.id)
        .copied()
        .unwrap_or_default();
    let can_manage = AccessPolicy::viewer_can_manage_artifact(&view.viewer, artifact);
    let is_owned_by_viewer = !is_admin && can_manage;
    let views = view.view_counts.get(&artifact.id);
    let who = if artifact.uploader_label.is_empty() {
        artifact.client_id.0.clone()
    } else {
        artifact.uploader_label.clone()
    };
    let thumbnail_src = if artifact.body_sha256.is_empty() {
        format!("/thumbnails/{}", artifact.id.0)
    } else {
        format!("/thumbnails/{}?v={}", artifact.id.0, artifact.body_sha256)
    };
    let category = js_trim(&artifact.category).to_owned();
    let category_label = if category.is_empty() {
        "Uncategorized".to_owned()
    } else {
        category.clone()
    };
    let updated = modified(artifact);
    let search_text = format!(
        "{} {} {} {} {} {}",
        artifact.title, artifact.org.0, category, who, artifact.client_id.0, artifact.description
    )
    .to_lowercase();
    CardTemplate {
        id: artifact.id.0.clone(),
        org: artifact.org.0.clone(),
        title: artifact.title.clone(),
        description: artifact.description.clone(),
        has_description: !artifact.description.is_empty(),
        who,
        is_bundle: artifact.is_bundle,
        hidden: artifact.hidden,
        favorite: reaction.favorite != 0,
        vote: reaction.vote,
        needs_review: if is_admin {
            sentiment.down > 0
        } else {
            reaction.vote < 0
        },
        is_owned_by_viewer,
        show_visibility: can_manage,
        show_delete: can_manage,
        category: category.clone(),
        has_category: !category.is_empty(),
        category_label,
        color: color_for(&artifact.org, &view.org_colors),
        thumbnail_src,
        search_text,
        updated_datetime: updated.replacen(' ', "T", 1),
        updated_label: relative_time(updated, now),
        bytes_label: format_bytes(artifact.bytes),
        show_admin: is_admin,
        sentiment_favorites: sentiment.favorites,
        sentiment_up: sentiment.up,
        sentiment_down: sentiment.down,
        has_views: views.is_some(),
        views: views.map_or(0, |counts| counts.views),
        unique_viewers: views.map_or(0, |counts| counts.unique_viewers),
        unique_viewers_plural: views.is_none_or(|counts| counts.unique_viewers != 1),
        category_options: category_names
            .iter()
            .filter(|candidate| !candidate.is_empty())
            .map(|candidate| MoveOptionTemplate {
                value: candidate.clone(),
                label: candidate.clone(),
                selected: candidate.as_str() == category.as_str(),
            })
            .collect(),
        org_options: org_names
            .iter()
            .filter(|org| org.as_str() != artifact.org.0)
            .map(|org| MoveOptionTemplate {
                value: org.clone(),
                label: org.clone(),
                selected: false,
            })
            .collect(),
    }
}

fn modified(artifact: &ArtifactMeta) -> &str {
    if artifact.updated_at.0.is_empty() {
        &artifact.created_at.0
    } else {
        &artifact.updated_at.0
    }
}

fn category_cmp(left: &str, right: &str) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => left
            .to_lowercase()
            .encode_utf16()
            .cmp(right.to_lowercase().encode_utf16()),
    }
}

fn color_for(org: &OrgId, colors: &std::collections::BTreeMap<OrgId, Option<String>>) -> String {
    org_color(&org.0, colors.get(org).and_then(Option::as_deref))
}

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

fn valid_hex_color(value: &str) -> bool {
    matches!(value.len(), 4 | 7)
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

fn body_version(artifact: &ArtifactMeta) -> String {
    let value = if artifact.body_sha256.is_empty() {
        if artifact.revision == 0 {
            String::new()
        } else {
            artifact.revision.to_string()
        }
    } else {
        artifact.body_sha256.clone()
    };
    utf16_slice(&value, 12)
}

fn utf16_slice(value: &str, units: usize) -> String {
    String::from_utf16_lossy(&value.encode_utf16().take(units).collect::<Vec<_>>())
}

fn format_date(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() < 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return String::new();
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = usize::from(bytes[5] - b'0') * 10 + usize::from(bytes[6] - b'0');
    let day = usize::from(bytes[8] - b'0') * 10 + usize::from(bytes[9] - b'0');
    let month_name = month
        .checked_sub(1)
        .and_then(|index| MONTHS.get(index))
        .copied()
        .unwrap_or("undefined");
    format!("{month_name} {day}, {}", &value[..4])
}

fn relative_time(value: &str, now: i64) -> String {
    let parsed = if value.len() == 10 {
        format!("{value}T00:00:00Z")
    } else {
        format!("{}Z", value.replacen(' ', "T", 1))
    };
    let Some(timestamp) = OffsetDateTime::parse(&parsed, &Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
    else {
        return "Just now".to_owned();
    };
    let seconds = now.saturating_sub(timestamp).max(0);
    match seconds {
        0..=59 => "Just now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        86_400..=604_799 => format!("{}d ago", seconds / 86_400),
        _ => format_date(value),
    }
}

fn format_bytes(bytes: u64) -> String {
    let value = bytes as f64;
    if value < 1_024.0 {
        return format!("{bytes} B");
    }
    if value < 1_048_576.0 {
        return format!("{} KB", (value / 1_024.0).round().max(1.0) as u64);
    }
    let tenths = (value / 1_048_576.0 * 10.0).round() as u64;
    format!("{}.{:01} MB", tenths / 10, tenths % 10)
}

fn notification_snippet(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut whitespace = false;
    for character in value.chars() {
        if is_js_whitespace(character) {
            whitespace = true;
        } else {
            if whitespace && !collapsed.is_empty() {
                collapsed.push(' ');
            }
            whitespace = false;
            collapsed.push(character);
        }
    }
    utf16_slice(js_trim(&collapsed), 120)
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                *byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(*byte));
        } else {
            use fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[derive(Template)]
#[template(path = "not-found.html")]
struct NotFoundTemplate<'a> {
    favicon: TrustedStatic,
    theme_boot: TrustedStatic,
    app_name: &'a str,
    app_brand: &'a str,
    site_host: &'a str,
    message: &'a str,
    css: TrustedStatic,
}

#[derive(Template)]
#[template(path = "not-signed-in.html")]
struct NotSignedInTemplate<'a> {
    favicon: TrustedStatic,
    theme_boot: TrustedStatic,
    app_name: &'a str,
    app_brand: &'a str,
    site_host: &'a str,
    css: TrustedStatic,
}

#[derive(Template)]
#[template(path = "access-retry.html")]
struct AccessRetryTemplate<'a> {
    favicon: TrustedStatic,
    theme_boot: TrustedStatic,
    app_name: &'a str,
    app_brand: &'a str,
    site_host: &'a str,
    target: &'a str,
    css: TrustedStatic,
}

fn render<T: Template>(template: &T) -> Result<String, AppError> {
    template.render().map_err(|_| AppError::Internal)
}

impl PageRenderer for AskamaPageRenderer {
    fn gallery(&self, view: &GalleryView) -> Result<String, AppError> {
        render(&gallery_template(self, view))
    }

    fn shell(&self, view: &ShellView) -> Result<String, AppError> {
        render(&shell_template(self, view)?)
    }

    fn settings(&self, view: &SettingsView) -> Result<String, AppError> {
        super::settings::render_settings(&self.site_host, &self.app_name, &self.app_brand, view)
    }

    fn not_found(&self, message: Option<&str>) -> Result<String, AppError> {
        render(&NotFoundTemplate {
            favicon: FAVICON,
            theme_boot: THEME_BOOT,
            app_name: &self.app_name,
            app_brand: &self.app_brand,
            site_host: &self.site_host,
            message: message
                .filter(|message| !message.is_empty())
                .unwrap_or("It may have been deleted, or the link is no longer valid."),
            css: NOT_FOUND_CSS,
        })
    }

    fn not_signed_in(&self) -> Result<String, AppError> {
        render(&NotSignedInTemplate {
            favicon: FAVICON,
            theme_boot: THEME_BOOT,
            app_name: &self.app_name,
            app_brand: &self.app_brand,
            site_host: &self.site_host,
            css: NOT_SIGNED_IN_CSS,
        })
    }

    fn access_retry(&self, target: &str) -> Result<String, AppError> {
        render(&AccessRetryTemplate {
            favicon: FAVICON,
            theme_boot: THEME_BOOT,
            app_name: &self.app_name,
            app_brand: &self.app_brand,
            site_host: &self.site_host,
            target: if target.is_empty() {
                "/?cf_access_retry=1"
            } else {
                target
            },
            css: ACCESS_RETRY_CSS,
        })
    }
}

#[cfg(test)]
mod portal_characterization_tests {
    use std::collections::BTreeMap;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        config::AppConfig,
        model::{
            ArtifactId, ClientId, EmailAddress, Feedback, FeedbackAuthor, FeedbackId, OrgArtifacts,
            Timestamp, Viewer,
        },
        security::access::AccessPolicy,
    };

    fn meta(id: &str, org: &str, category: &str) -> ArtifactMeta {
        ArtifactMeta {
            id: ArtifactId::from(id),
            client_id: ClientId::from("publisher"),
            org: OrgId::from(org),
            title: format!("Title {id}"),
            description: "Description".to_owned(),
            bytes: 1_310_720,
            created_at: Timestamp("2026-01-01 00:00:00".to_owned()),
            updated_at: Timestamp("2026-01-02 00:00:00".to_owned()),
            uploader_label: "Uploader".to_owned(),
            owner_email: Some("viewer@example.test".to_owned()),
            is_bundle: false,
            entry: String::new(),
            revision: 3,
            category: category.to_owned(),
            hidden: false,
            body_sha256: "deadbeefcafebabe".to_owned(),
            artifact_key: None,
        }
    }

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn gallery_html_bytes_match_original_member_and_admin_variants() {
        let renderer = AskamaPageRenderer::with_fixed_clock(&AppConfig::default(), 1_767_232_800);
        let artifact = meta("artifact1234", "acme", "Reports");
        let member = GalleryView {
            viewer: Viewer {
                email: Some(EmailAddress::from("viewer@example.test")),
                org: Some(OrgId::from("acme")),
                is_admin: false,
            },
            sections: vec![OrgArtifacts {
                org: OrgId::from("acme"),
                items: vec![artifact.clone()],
            }],
            reactions: BTreeMap::from([(
                artifact.id.clone(),
                crate::model::Reaction {
                    favorite: 1,
                    vote: -1,
                },
            )]),
            sentiment: BTreeMap::new(),
            view_counts: BTreeMap::new(),
            top_viewed: BTreeMap::new(),
            org_colors: BTreeMap::from([(OrgId::from("acme"), Some("#123456".to_owned()))]),
            org_categories: BTreeMap::from([(
                OrgId::from("acme"),
                vec!["Reports".to_owned(), "Runbooks".to_owned()],
            )]),
            notifications: Vec::new(),
            unread_notifications: 0,
        };
        let mut admin = member.clone();
        admin.viewer = Viewer {
            email: Some(EmailAddress::from("admin@example.test")),
            org: Some(OrgId::from("admin")),
            is_admin: true,
        };
        admin.sentiment.insert(
            artifact.id.clone(),
            crate::model::Sentiment {
                up: 2,
                down: 1,
                favorites: 1,
            },
        );

        assert_eq!(
            digest(
                renderer
                    .gallery(&member)
                    .expect("member gallery")
                    .as_bytes()
            ),
            "0d333afc2deaa2ddc180bff61f65fe627b4604c48a1e718ee05dd9f9d64d5203"
        );
        assert_eq!(
            digest(renderer.gallery(&admin).expect("admin gallery").as_bytes()),
            "dce5f34b3321b28edf55ee25e7894abcaa2a63922780ffb7eeff503182ae8407"
        );
    }

    #[test]
    fn shell_html_bytes_match_original_authenticated_bundle_variant() {
        let renderer = AskamaPageRenderer::with_fixed_clock(&AppConfig::default(), 1_767_232_800);
        let viewer = Viewer {
            email: Some(EmailAddress::from("viewer@example.test")),
            org: Some(OrgId::from("acme")),
            is_admin: false,
        };
        let mut artifact = meta("artifact1234", "acme", "Reports");
        artifact.is_bundle = true;
        let authorized = AccessPolicy::authorize_viewer(&viewer, Some(artifact.clone()))
            .expect("authorized fixture");
        let feedback = Feedback {
            id: FeedbackId::from("feedback-1"),
            artifact_id: artifact.id,
            org: OrgId::from("acme"),
            parent_id: None,
            viewer_email: viewer.email.clone(),
            author: FeedbackAuthor::Artifact {
                viewer_email: EmailAddress::from("viewer@example.test"),
            },
            body: "A comment".to_owned(),
            artifact_revision: 3,
            anchor_path: Some("main".to_owned()),
            anchor_x: Some(0.1),
            anchor_y: Some(0.2),
            anchor_w: Some(0.3),
            anchor_h: Some(0.4),
            anchor_approx: false,
            anchor_page: Some("1".to_owned()),
            anchor_kind: Some("section".to_owned()),
            anchor_node_id: Some("node".to_owned()),
            anchor_quote: Some("quote".to_owned()),
            anchor_version: 2,
            created_at: Timestamp("2026-01-01 00:00:00".to_owned()),
            resolved_at: None,
            resolved_by: None,
            external_created_at: None,
            external_edited_at: None,
            external_deleted_at: None,
        };
        let view = ShellView {
            artifact: authorized,
            navigation: crate::render::view_models::ArtifactNavigation {
                previous_id: Some(ArtifactId::from("previous")),
                next_id: Some(ArtifactId::from("next")),
                index: 2,
                total: 3,
            },
            reaction: crate::model::Reaction {
                favorite: 1,
                vote: -1,
            },
            feedback: vec![feedback],
            view_counts: crate::model::ViewCounts {
                views: 5,
                unique_viewers: 2,
                last_viewed_at: None,
            },
            viewers: None,
            viewer,
            viewer_display_name: Some("Viewer".to_owned()),
            org_accent: Some("#123456".to_owned()),
            cast_enabled: true,
        };
        assert_eq!(
            digest(renderer.shell(&view).expect("bundle shell").as_bytes()),
            "5cab175fa6d620e59ec1916d78bbfcd03eea89e55112cdb90a07945c37261acf"
        );
    }
}
