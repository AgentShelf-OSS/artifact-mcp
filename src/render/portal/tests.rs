use super::*;

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
        hex::encode(Sha256::digest(bytes))
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
