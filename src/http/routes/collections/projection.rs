use serde_json::{Value, json};

use crate::{
    AppDeps,
    error::AppError,
    model::OrgId,
    ports::collections::{CollectionActor, MAX_COLLECTIONS_PER_ORG},
};

pub(super) async fn projection(
    deps: &AppDeps,
    actor: &CollectionActor,
    orgs: &[String],
) -> Result<Value, AppError> {
    let registered = deps.admin.org_names().await?;
    if orgs
        .iter()
        .any(|org| !registered.iter().any(|entry| &entry.0 == org))
    {
        return Err(AppError::ConcealedNotFound);
    }
    let store = deps.collections.as_ref();
    let mut rows = Vec::new();
    for org in orgs {
        rows.extend(
            store
                .list_for_org(actor, org.clone(), MAX_COLLECTIONS_PER_ORG)
                .await?,
        );
    }
    let mut projected = Vec::new();
    let mut visible = std::collections::HashMap::new();
    for org in orgs {
        let items = deps
            .artifacts
            .list_org_artifacts(&OrgId(org.clone()), true)
            .await?;
        visible.extend(
            items
                .into_iter()
                .filter(|a| {
                    actor.is_admin
                        || !a.hidden
                        || a.owner_email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(&actor.email))
                })
                .map(|a| {
                    let view = crate::model::ArtifactPublicView::from(a);
                    (view.id.0.clone(), view)
                }),
        );
    }
    let mut collected = std::collections::HashSet::new();
    for row in rows {
        let mut row_actor = actor.clone();
        row_actor.org = row.org.clone();
        let ids = store
            .members(&row_actor, row.id.clone())
            .await?
            .into_iter()
            .filter(|id| visible.contains_key(id))
            .collect::<Vec<_>>();
        collected.extend(ids.iter().cloned());
        let cover = row.cover_artifact_id.filter(|id| visible.contains_key(id));
        let mut preview_ids = cover.iter().cloned().collect::<Vec<_>>();
        preview_ids.extend(
            ids.iter()
                .filter(|id| Some(*id) != cover.as_ref())
                .take(3usize.saturating_sub(preview_ids.len()))
                .cloned(),
        );
        let previews = preview_ids
            .iter()
            .map(|id| preview_artifact(id, visible.get(id)))
            .collect::<Vec<_>>();
        projected.push(json!({"id":row.id,"org":row.org,"name":row.name,"description":row.description,"color":row.color.filter(|color|!color.is_empty()),"createdBy":row.created_by,"editable":actor.is_admin || (row.created_by_kind == "email" && row.created_by.eq_ignore_ascii_case(&actor.email)),"artifactCount":ids.len(),"artifactIds":ids,"coverArtifactId":cover,"previewArtifacts":previews}));
    }
    Ok(
        json!({"collections":projected,"uncollectedCount":visible.len().saturating_sub(collected.len()),"preferences":store.preferences(actor).await?}),
    )
}

// Keep the empty-version query parameter for artifacts with no digest.
fn preview_artifact(id: &str, meta: Option<&crate::model::ArtifactPublicView>) -> Value {
    json!({"id":id,"thumbnail":format!("/thumbnails/{}?v={}",id,meta.map(|a|a.details.content_digest.clone()).unwrap_or_default())})
}

#[cfg(test)]
mod public_projection_tests {
    use super::*;

    #[test]
    fn public_projection_preview_bytes() {
        let mut meta = crate::model::ArtifactMeta::fixture("a", "acme");
        meta.body_sha256 = "0123456789abcdef".repeat(4);
        assert_eq!(
            preview_artifact(
                "a",
                Some(&crate::model::ArtifactPublicView::from_meta(&meta))
            )
            .to_string(),
            r#"{"id":"a","thumbnail":"/thumbnails/a?v=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}"#
        );
        assert_eq!(
            preview_artifact("a", None).to_string(),
            r#"{"id":"a","thumbnail":"/thumbnails/a?v="}"#
        );
        meta.body_sha256.clear();
        assert_eq!(
            preview_artifact(
                "a",
                Some(&crate::model::ArtifactPublicView::from_meta(&meta))
            )
            .to_string(),
            r#"{"id":"a","thumbnail":"/thumbnails/a?v="}"#
        );
    }
}
