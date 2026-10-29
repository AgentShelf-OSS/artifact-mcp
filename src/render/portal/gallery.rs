use super::*;

pub(super) fn gallery_template<'a>(
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

pub(super) struct GalleryCollection<'a> {
    is_admin: bool,
    org_names: Vec<String>,
    artifacts: Vec<&'a ArtifactMeta>,
    organization_total: usize,
    favorite_total: usize,
    needs_review_total: usize,
    hidden_total: usize,
    chips: Vec<GalleryChipTemplate>,
}

pub(super) fn gallery_collection(view: &GalleryView) -> GalleryCollection<'_> {
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

pub(super) fn gallery_categories(
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

pub(super) fn gallery_cards(
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

pub(super) fn gallery_notifications(view: &GalleryView, now: i64) -> Vec<NotificationTemplate> {
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

pub(super) fn gallery_identity(view: &GalleryView, is_admin: bool) -> (String, String) {
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

pub(super) fn card_template(
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
