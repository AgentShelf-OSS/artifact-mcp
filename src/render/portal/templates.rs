use super::*;

#[derive(Clone, Copy)]
pub(super) struct Icons {
    pub(super) search: TrustedStatic,
    pub(super) bell: TrustedStatic,
    pub(super) theme: TrustedStatic,
    pub(super) signout: TrustedStatic,
    pub(super) open: TrustedStatic,
    pub(super) download: TrustedStatic,
    pub(super) heart: TrustedStatic,
    pub(super) up: TrustedStatic,
    pub(super) down: TrustedStatic,
    pub(super) back: TrustedStatic,
    pub(super) forward: TrustedStatic,
    pub(super) eye: TrustedStatic,
    pub(super) eye_off: TrustedStatic,
    pub(super) comment: TrustedStatic,
    pub(super) share: TrustedStatic,
    pub(super) more: TrustedStatic,
}

pub(super) const ICONS: Icons = Icons {
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
pub(super) struct GalleryTemplate<'a> {
    pub(super) collection_toolbar_css: TrustedStatic,
    pub(super) collection_toolbar_script: TrustedStatic,
    pub(super) collection_css: TrustedStatic,
    pub(super) collection_script: TrustedStatic,
    pub(super) favicon: TrustedStatic,
    pub(super) theme_boot: TrustedStatic,
    pub(super) css: TrustedStatic,
    pub(super) script: TrustedStatic,
    pub(super) icons: Icons,
    pub(super) app_name: &'a str,
    pub(super) app_brand: &'a str,
    pub(super) site_host: &'a str,
    pub(super) viewer_email: String,
    pub(super) viewer_is_admin: bool,
    pub(super) viewer_org: String,
    pub(super) role: String,
    pub(super) identity_color: String,
    pub(super) total: usize,
    pub(super) organization_total: usize,
    pub(super) favorite_total: usize,
    pub(super) needs_review_total: usize,
    pub(super) hidden_total: usize,
    pub(super) has_delete_actions: bool,
    pub(super) cards: Vec<CardTemplate>,
    pub(super) chips: Vec<GalleryChipTemplate>,
    pub(super) categories: Vec<CategoryFilterTemplate>,
    pub(super) org_category_json: String,
    pub(super) notifications: Vec<NotificationTemplate>,
    pub(super) unread_notifications: u64,
}

pub(super) struct GalleryChipTemplate {
    pub(super) org: String,
    pub(super) count: usize,
}

pub(super) struct NotificationTemplate {
    pub(super) href: String,
    pub(super) unread: bool,
    pub(super) viewer_email: String,
    pub(super) relative_time: String,
    pub(super) artifact_title: String,
    pub(super) snippet: String,
}

pub(super) struct CategoryFilterTemplate {
    pub(super) key: String,
    pub(super) label: String,
    pub(super) count: usize,
}

#[derive(Clone, Serialize)]
pub(super) struct OrgCategoryTemplate {
    pub(super) name: String,
    pub(super) count: usize,
}

pub(super) struct CardTemplate {
    pub(super) id: String,
    pub(super) org: String,
    pub(super) title: String,
    pub(super) description: String,
    pub(super) has_description: bool,
    pub(super) who: String,
    pub(super) is_bundle: bool,
    pub(super) hidden: bool,
    pub(super) favorite: bool,
    pub(super) vote: i8,
    pub(super) needs_review: bool,
    pub(super) is_owned_by_viewer: bool,
    pub(super) show_visibility: bool,
    pub(super) show_delete: bool,
    pub(super) category: String,
    pub(super) has_category: bool,
    pub(super) category_label: String,
    pub(super) color: String,
    pub(super) thumbnail_src: String,
    pub(super) search_text: String,
    pub(super) updated_datetime: String,
    pub(super) updated_label: String,
    pub(super) bytes_label: String,
    pub(super) show_admin: bool,
    pub(super) sentiment_favorites: u64,
    pub(super) sentiment_up: u64,
    pub(super) sentiment_down: u64,
    pub(super) has_views: bool,
    pub(super) views: u64,
    pub(super) unique_viewers: u64,
    pub(super) unique_viewers_plural: bool,
    pub(super) category_options: Vec<MoveOptionTemplate>,
    pub(super) org_options: Vec<MoveOptionTemplate>,
}

pub(super) struct MoveOptionTemplate {
    pub(super) value: String,
    pub(super) label: String,
    pub(super) selected: bool,
}

pub(super) const SHELL_CSS: TrustedStatic = TrustedStatic::new(concat!(
    include_str!("../../../assets/dialogs.css"),
    "\n",
    include_str!("../../../assets/shell.css")
));
/// The viewer shell script: `assets/dialogs.js`, a newline, then the `assets/shell/` parts in the
/// order `assets/shell/parts.txt` lists. The parts are one closure split across files, so each
/// part alone is not valid JavaScript. A test keeps this list and `parts.txt` in step.
pub(super) const SHELL_SCRIPT: TrustedStatic = TrustedStatic::new(concat!(
    include_str!("../../../assets/dialogs.js"),
    "\n",
    include_str!("../../../assets/shell/00-prelude.js"),
    include_str!("../../../assets/shell/01-state-broker.js"),
    include_str!("../../../assets/shell/02-geo-broker.js"),
    include_str!("../../../assets/shell/03-native-broker.js"),
    include_str!("../../../assets/shell/04-data-broker.js"),
    include_str!("../../../assets/shell/05-reminder-broker.js"),
    include_str!("../../../assets/shell/06-reactions-inspector.js"),
    include_str!("../../../assets/shell/07-comment-bridge-wiring.js"),
    include_str!("../../../assets/shell/08-speech-broker.js"),
    include_str!("../../../assets/shell/09-reader-ui.js"),
    include_str!("../../../assets/shell/10-reader-place.js"),
    include_str!("../../../assets/shell/11-reader-host.js"),
    include_str!("../../../assets/shell/12-reader-ahead.js"),
    include_str!("../../../assets/shell/13-reader-controls.js"),
    include_str!("../../../assets/shell/14-reader-voices.js"),
    include_str!("../../../assets/shell/15-anchors-comments.js"),
    include_str!("../../../assets/shell/16-frame-wiring.js"),
    include_str!("../../../assets/shell/17-discussion-category.js"),
    include_str!("../../../assets/shell/18-history-audience.js"),
));
pub(super) const ACTION_SCRIPT: TrustedStatic =
    TrustedStatic::new(include_str!("../../../assets/actions.js"));
pub(super) const CAST_SCRIPT: TrustedStatic =
    TrustedStatic::new(include_str!("../../../assets/cast.js"));

#[derive(Template)]
#[template(path = "artifact-shell.html")]
pub(super) struct ShellTemplate<'a> {
    pub(super) viewer_boot: TrustedStatic,
    pub(super) favicon: TrustedStatic,
    pub(super) theme_boot: TrustedStatic,
    pub(super) css: TrustedStatic,
    pub(super) script: TrustedStatic,
    pub(super) cast_script: TrustedStatic,
    pub(super) action_script: TrustedStatic,
    pub(super) cast_enabled: bool,
    pub(super) icons: Icons,
    pub(super) app_name: &'a str,
    pub(super) title: String,
    pub(super) org: String,
    pub(super) color: String,
    pub(super) who: String,
    pub(super) is_bundle: bool,
    pub(super) category: String,
    pub(super) has_category: bool,
    pub(super) favorite: bool,
    pub(super) vote: i8,
    pub(super) previous_id: String,
    pub(super) has_previous: bool,
    pub(super) next_id: String,
    pub(super) has_next: bool,
    pub(super) navigation_index: usize,
    pub(super) navigation_total: usize,
    pub(super) views: u64,
    pub(super) has_viewers: bool,
    pub(super) unresolved: usize,
    pub(super) revision: u64,
    pub(super) raw_src: String,
    pub(super) anchor_raw_src: String,
    pub(super) threads: Vec<FeedbackThreadTemplate>,
    pub(super) viewers: Vec<ViewerTemplate>,
    pub(super) artifact_id: String,
    /// Home Screen title; see [`artifact_install_name`].
    pub(super) install_name: String,
    pub(super) artifact_id_literal: String,
    pub(super) previous_id_literal: String,
    pub(super) next_id_literal: String,
    pub(super) bundle_raw_prefix_literal: String,
    pub(super) version_query_literal: String,
    pub(super) viewer_email_literal: String,
    pub(super) viewer_email: String,
    pub(super) viewer_id_literal: String,
    pub(super) viewer_name_literal: String,
    pub(super) viewer_is_admin: bool,
    pub(super) state_enabled: bool,
    pub(super) can_delete: bool,
    pub(super) feedback_literal: String,
    pub(super) title_literal: String,
    pub(super) bytes: u64,
}

pub(super) struct FeedbackThreadTemplate {
    pub(super) parent: FeedbackTemplate,
    pub(super) replies: Vec<FeedbackTemplate>,
}

pub(super) struct FeedbackTemplate {
    pub(super) id: String,
    pub(super) viewer_email: String,
    pub(super) body: String,
    pub(super) created_at: String,
    pub(super) resolved: bool,
    pub(super) manageable: bool,
    pub(super) anchor_state: String,
    pub(super) has_anchor_state: bool,
}

pub(super) struct ViewerTemplate {
    pub(super) email: String,
    pub(super) count: u64,
    pub(super) count_plural: bool,
    pub(super) last_seen: String,
}

#[derive(Serialize)]
pub(super) struct FeedbackScriptRow<'a> {
    pub(super) id: &'a str,
    pub(super) parent_id: Option<&'a str>,
    pub(super) viewer_email: &'a str,
    pub(super) author_source: Option<&'a str>,
    pub(super) external_author_display: Option<&'a str>,
    pub(super) body: &'a str,
    pub(super) created_at: &'a str,
    pub(super) resolved_at: Option<&'a str>,
    pub(super) anchor_path: Option<&'a str>,
    pub(super) anchor_x: Option<f64>,
    pub(super) anchor_y: Option<f64>,
    pub(super) anchor_w: Option<f64>,
    pub(super) anchor_h: Option<f64>,
    pub(super) anchor_approx: bool,
    pub(super) anchor_page: Option<&'a str>,
    pub(super) anchor_page_stale: bool,
    pub(super) anchor_kind: Option<&'a str>,
    pub(super) anchor_node_id: Option<&'a str>,
    pub(super) anchor_quote: Option<&'a str>,
    pub(super) anchor_version: u8,
    pub(super) artifact_revision: u64,
}

/// Fallback install name when an artifact title has no printable text.
pub(super) const INSTALL_NAME_FALLBACK: &str = "Artifact";

/// Remove control characters, trim, keep at most `limit` code points, and trim the end again.
///
/// Mirrors Node's `installName` in `lib/portal.js`: `char::is_control` is the Unicode `Cc`
/// category (U+0000-U+001F and U+007F-U+009F) and `char::is_whitespace` is `White_Space`,
/// matching `\p{White_Space}`. Counting `char`s never splits a code point.
#[derive(Template)]
#[template(path = "not-found.html")]
pub(super) struct NotFoundTemplate<'a> {
    pub(super) favicon: TrustedStatic,
    pub(super) theme_boot: TrustedStatic,
    pub(super) app_name: &'a str,
    pub(super) app_brand: &'a str,
    pub(super) site_host: &'a str,
    pub(super) message: &'a str,
    pub(super) css: TrustedStatic,
}

#[derive(Template)]
#[template(path = "not-signed-in.html")]
pub(super) struct NotSignedInTemplate<'a> {
    pub(super) favicon: TrustedStatic,
    pub(super) theme_boot: TrustedStatic,
    pub(super) app_name: &'a str,
    pub(super) app_brand: &'a str,
    pub(super) site_host: &'a str,
    pub(super) css: TrustedStatic,
}

#[derive(Template)]
#[template(path = "access-retry.html")]
pub(super) struct AccessRetryTemplate<'a> {
    pub(super) favicon: TrustedStatic,
    pub(super) theme_boot: TrustedStatic,
    pub(super) app_name: &'a str,
    pub(super) app_brand: &'a str,
    pub(super) site_host: &'a str,
    pub(super) target: &'a str,
    pub(super) css: TrustedStatic,
}
