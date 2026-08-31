//! Askama-backed gallery, viewer shell, and standalone pages.

use std::{
    cmp::Ordering,
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
}

impl AskamaPageRenderer {
    #[must_use]
    pub fn from_config(config: &AppConfig) -> Self {
        Self::new(
            config.app_name.clone(),
            config.app_brand.clone(),
            &config.public_base_url,
        )
    }

    #[must_use]
    pub fn new(app_name: String, app_brand: String, public_base_url: &str) -> Self {
        Self {
            app_name,
            app_brand,
            site_host: site_host(public_base_url),
            clock: Arc::new(system_now_unix_seconds),
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
    favorite_total: usize,
    needs_review_total: usize,
    show_chips: bool,
    has_delete_actions: bool,
    cards: Vec<CardTemplate>,
    chips: Vec<GalleryChipTemplate>,
    categories: Vec<CategoryFilterTemplate>,
    notifications: Vec<NotificationTemplate>,
    unread_notifications: u64,
}

struct GalleryChipTemplate {
    org: String,
    color: String,
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
}

const SHELL_CSS: TrustedStatic = TrustedStatic::new(include_str!("../../assets/shell.css"));
const SHELL_SCRIPT: TrustedStatic = TrustedStatic::new(include_str!("../../assets/shell.js"));

#[derive(Template)]
#[template(path = "artifact-shell.html")]
struct ShellTemplate<'a> {
    favicon: TrustedStatic,
    theme_boot: TrustedStatic,
    css: TrustedStatic,
    script: TrustedStatic,
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
    artifact_id_literal: String,
    previous_id_literal: String,
    next_id_literal: String,
    bundle_raw_prefix_literal: String,
    version_query_literal: String,
    viewer_email_literal: String,
    viewer_is_admin: bool,
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
    anchor_path: Option<&'a str>,
    anchor_x: Option<f64>,
    anchor_y: Option<f64>,
    anchor_w: Option<f64>,
    anchor_h: Option<f64>,
    anchor_approx: bool,
    anchor_page: Option<&'a str>,
    anchor_page_stale: bool,
    artifact_revision: u64,
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
    let who = if meta.uploader_label.is_empty() {
        meta.client_id.0.clone()
    } else {
        meta.uploader_label.clone()
    };
    let threads = view
        .feedback
        .iter()
        .filter(|feedback| feedback.parent_id.is_none())
        .map(|parent| FeedbackThreadTemplate {
            parent: feedback_template(parent, meta.revision, viewer_email, view.viewer.is_admin),
            replies: view
                .feedback
                .iter()
                .filter(|reply| reply.parent_id.as_ref() == Some(&parent.id))
                .map(|reply| {
                    feedback_template(reply, meta.revision, viewer_email, view.viewer.is_admin)
                })
                .collect(),
        })
        .collect();
    let feedback_json = serde_json::to_string(
        &view
            .feedback
            .iter()
            .map(|feedback| FeedbackScriptRow {
                id: &feedback.id.0,
                parent_id: feedback.parent_id.as_ref().map(|id| id.0.as_str()),
                anchor_path: feedback.anchor_path.as_deref(),
                anchor_x: feedback.anchor_x,
                anchor_y: feedback.anchor_y,
                anchor_w: feedback.anchor_w,
                anchor_h: feedback.anchor_h,
                anchor_approx: feedback.anchor_approx,
                anchor_page: feedback.anchor_page.as_deref(),
                // The frozen Feedback model does not expose Node's projection-only stale flag.
                anchor_page_stale: false,
                artifact_revision: feedback.artifact_revision,
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|_| AppError::Internal)?;
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
    let viewers = view
        .viewers
        .as_ref()
        .into_iter()
        .flatten()
        .map(|viewer| ViewerTemplate {
            email: viewer.email.0.clone(),
            count: viewer.count,
            count_plural: viewer.count != 1,
            last_seen: format_date(&viewer.last_viewed_at.0),
        })
        .collect();
    Ok(ShellTemplate {
        favicon: FAVICON,
        theme_boot: THEME_BOOT,
        css: SHELL_CSS,
        script: SHELL_SCRIPT,
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
        anchor_raw_src: format!("{raw_src}?anchor=1{version_query}"),
        threads,
        viewers,
        artifact_id: meta.id.0.clone(),
        artifact_id_literal: js_literal(&meta.id.0),
        previous_id_literal: js_literal(&previous_id),
        next_id_literal: js_literal(&next_id),
        bundle_raw_prefix_literal: js_literal(&format!("/raw/{}/", meta.id.0)),
        version_query_literal: js_literal(&version_query),
        viewer_email_literal: js_literal(viewer_email),
        viewer_is_admin: view.viewer.is_admin,
        can_delete: AccessPolicy::viewer_can_manage_artifact(&view.viewer, meta),
        feedback_literal: js_literal(&feedback_json),
        title_literal: js_literal(&meta.title),
        bytes: meta.bytes,
    })
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
    let is_admin = view.viewer.is_admin;
    let org_names: Vec<String> = view
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
    let total = artifacts.len();
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
    let chips = view
        .sections
        .iter()
        .map(|section| GalleryChipTemplate {
            org: section.org.0.clone(),
            color: color_for(&section.org, &view.org_colors),
            count: section.items.len(),
        })
        .collect();
    let mut category_counts: Vec<(String, usize)> = Vec::new();
    for artifact in &artifacts {
        let key = js_trim(&artifact.category).to_owned();
        if let Some((_, count)) = category_counts.iter_mut().find(|(name, _)| name == &key) {
            *count += 1;
        } else {
            category_counts.push((key, 1));
        }
    }
    category_counts.sort_by(|(left, _), (right, _)| category_cmp(left, right));
    let category_names: Vec<String> = category_counts
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let categories = category_counts
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
    let now = (renderer.clock)();
    let cards: Vec<CardTemplate> = artifacts
        .into_iter()
        .map(|artifact| card_template(artifact, view, &org_names, &category_names, is_admin, now))
        .collect();
    let has_delete_actions = cards.iter().any(|card| card.show_delete);
    let notifications = view
        .notifications
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
        .collect();
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
    GalleryTemplate {
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
        viewer_is_admin: is_admin,
        viewer_org: if viewer_org.is_empty() {
            "your organization".to_owned()
        } else {
            viewer_org.clone()
        },
        role: if is_admin {
            "All organizations".to_owned()
        } else if viewer_org.is_empty() {
            "Member".to_owned()
        } else {
            viewer_org.clone()
        },
        identity_color: org_color(identity_org, identity_color),
        total,
        favorite_total,
        needs_review_total,
        show_chips: view.sections.len() > 1,
        has_delete_actions,
        cards,
        chips,
        categories,
        notifications,
        unread_notifications: view.unread_notifications,
    }
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
            .filter(|candidate| candidate.as_str() != category)
            .map(|category| MoveOptionTemplate {
                value: category.clone(),
                label: if category.is_empty() {
                    "Uncategorized".to_owned()
                } else {
                    category.clone()
                },
            })
            .collect(),
        org_options: org_names
            .iter()
            .filter(|org| org.as_str() != artifact.org.0)
            .map(|org| MoveOptionTemplate {
                value: org.clone(),
                label: org.clone(),
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
            target: if target.is_empty() {
                "/?cf_access_retry=1"
            } else {
                target
            },
            css: ACCESS_RETRY_CSS,
        })
    }
}
