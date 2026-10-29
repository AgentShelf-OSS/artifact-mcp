use super::*;

pub(super) fn install_name(value: &str, limit: usize) -> String {
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

pub(super) fn shell_template<'a>(
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
        viewer_boot: TrustedStatic::new(include_str!("../../../assets/viewer-boot.js")),
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

pub(super) fn shell_viewer_identity(
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

pub(super) fn shell_feedback_threads(
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

pub(super) fn shell_feedback_json(view: &ShellView) -> Result<String, AppError> {
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

pub(super) fn shell_navigation(view: &ShellView) -> (String, String) {
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

pub(super) fn shell_viewers(view: &ShellView) -> Vec<ViewerTemplate> {
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

pub(super) fn feedback_author_label(author: &crate::model::FeedbackAuthor) -> &str {
    match author {
        crate::model::FeedbackAuthor::Artifact { viewer_email } => &viewer_email.0,
        crate::model::FeedbackAuthor::Discord {
            external_author_display,
            ..
        } => external_author_display,
    }
}

pub(super) fn feedback_template(
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
