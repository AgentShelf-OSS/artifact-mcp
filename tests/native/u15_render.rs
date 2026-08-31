use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use artifact_mcp::{
    config::AppConfig,
    model::{
        ArtifactId, ArtifactMeta, ClientId, EmailAddress, Feedback, FeedbackAuthor, FeedbackId,
        OrgArtifacts, OrgId, Organization, PublisherKeySummary, Reaction, Timestamp, ViewCounts,
        Viewer, ViewerNotification, WebhookEvent, WebhookId, WebhookSummary,
    },
    ports::PageRenderer,
    render::{
        portal::{AskamaPageRenderer, js_literal},
        view_models::{
            ArtifactNavigation, GalleryView, SettingsOrganization, SettingsView, ShellView,
        },
    },
    security::access::AccessPolicy,
};
use serde_json::{Value, json};

const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const NODE_RENDER_DRIVER: &str = r#"
Promise.all([import(process.argv[1]), import(process.argv[2])]).then(([portal, settings]) => {
  const input = JSON.parse(process.argv[3]);
  Date.now = () => input.nowMs;
  const maps = (rows) => new Map(rows || []);
  const gallery = portal.renderGallery(
    input.viewer,
    input.sections,
    maps(input.reactions),
    maps(input.sentiment),
    maps(input.viewCounts),
    maps(input.topViewed),
    input.orgColors,
    input.notifications
  );
  const shell = portal.renderArtifactShell(
    input.meta,
    input.nav,
    input.reaction,
    input.feedback,
    input.analytics,
    input.viewer,
    input.orgAccent
  );
  const settingsHtml = settings.renderSettings(input.admin, input.keys, input.organizations);
  const match = (html, pattern) => { const found = html.match(pattern); return found ? found[1] : null; };
  const decodeAttr = (value) => value == null ? null : value
    .replaceAll('&quot;', '"').replaceAll('&#39;', "'")
    .replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
  const blocks = (html, tag) => Array.from(html.matchAll(new RegExp('<' + tag + '>([\\s\\S]*?)</' + tag + '>', 'g')), row => row[1]);
  const lastBlock = (html, tag) => { const found = blocks(html, tag); return found[found.length - 1] || null; };
  process.stdout.write(JSON.stringify({
    portalCss: portal.PORTAL_CSS,
    portalScript: lastBlock(gallery, 'script'),
    shellCss: lastBlock(shell, 'style'),
    settingsCss: lastBlock(settingsHtml, 'style'),
    settingsScript: lastBlock(settingsHtml, 'script'),
    orgColor: portal.orgColor(input.meta.org, input.orgAccent),
    sandbox: match(shell, /sandbox="([^"]+)"/),
    shellSrc: match(shell, /id="vframe" src="([^"]+)"/),
    viewerLiteral: decodeAttr(match(shell, /data-viewer-email="([^"]*)"/)),
    feedbackLiteral: decodeAttr(match(shell, /data-feedback="([^"]*)"/)),
    thumbnailSrc: match(gallery, /<img class="pv" src="([^"]+)"/),
    galleryHasBytes: gallery.includes('1.3 MB'),
    galleryHasJustNow: gallery.includes('Just now'),
    settingsEscaped: !settingsHtml.includes(input.attack) && settingsHtml.includes('Specific emails'),
    categoryFilters: Array.from(gallery.matchAll(/data-filter-category="([^"]*)"/g), row => row[1]).slice(1),
    cardOrder: Array.from(gallery.matchAll(/<article class="card[^"]*"[^>]*data-id="([^"]*)"/g), row => row[1]),
    notificationHref: match(gallery, /<a class="notif-row[^\"]*" href="([^"]+)"/)
  }));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

#[test]
fn standalone_pages_autoescape_request_derived_values() {
    let renderer = AskamaPageRenderer::from_config(&AppConfig::default());
    let attack = r#"</script><img src=x onerror='alert(1)'>&\"\u{2028}🎉"#;

    let not_found = renderer.not_found(Some(attack)).expect("render not found");
    assert!(!not_found.contains(attack));
    assert!(not_found.contains("&#60;/script&#62;&#60;img"));
    assert!(not_found.contains("&#39;alert(1)&#39;"));

    let retry = renderer.access_retry(attack).expect("render access retry");
    assert!(!retry.contains(&format!(r#"href="{attack}""#)));
    assert!(retry.contains("&#60;/script&#62;&#60;img"));
    assert!(retry.contains("&#38;\\&#34;"));

    let signed_out = renderer.not_signed_in().expect("render signed-out page");
    assert!(signed_out.contains("Sign in to view your organization’s artifacts."));
}

#[test]
fn gallery_renders_fixed_clock_state_and_escapes_hostile_metadata() {
    let renderer = AskamaPageRenderer::with_fixed_clock(&AppConfig::default(), 0);
    let attack = "</script>\"&\u{2028}🎉";
    let artifact = ArtifactMeta {
        id: ArtifactId::from("artifact1234"),
        client_id: ClientId::from("publisher"),
        org: OrgId::from("acme🎉"),
        title: attack.to_owned(),
        description: format!("description {attack}"),
        bytes: 1_310_720,
        created_at: Timestamp("1970-01-01 00:00:00".to_owned()),
        updated_at: Timestamp("1970-01-01 00:00:00".to_owned()),
        uploader_label: "Publisher \"One\"".to_owned(),
        owner_email: None,
        is_bundle: false,
        entry: String::new(),
        revision: 3,
        category: "Reports".to_owned(),
        hidden: false,
        body_sha256: "deadbeefcafebabe".to_owned(),
    };
    let notification = ViewerNotification {
        id: FeedbackId::from("feedback-1"),
        artifact_id: artifact.id.clone(),
        artifact_title: attack.to_owned(),
        org: artifact.org.clone(),
        body: format!("note {attack}"),
        author: FeedbackAuthor::Artifact {
            viewer_email: EmailAddress::from("author@example.test"),
        },
        created_at: Timestamp("1970-01-01 00:00:00".to_owned()),
        parent_id: None,
        resolved: false,
        has_anchor: false,
        unread: true,
    };
    let view = GalleryView {
        viewer: Viewer {
            email: Some(EmailAddress::from("viewer@example.test")),
            org: Some(artifact.org.clone()),
            is_admin: false,
        },
        sections: vec![OrgArtifacts {
            org: artifact.org.clone(),
            items: vec![artifact],
        }],
        reactions: BTreeMap::new(),
        sentiment: BTreeMap::new(),
        view_counts: BTreeMap::new(),
        top_viewed: BTreeMap::new(),
        org_colors: BTreeMap::new(),
        notifications: vec![notification],
        unread_notifications: 1,
    };

    let html = renderer.gallery(&view).expect("render gallery");
    assert!(!html.contains("</script>\"&"));
    assert!(html.contains("&#60;/script&#62;"));
    assert!(html.contains("1.3 MB"));
    assert!(html.contains("Just now"));
    assert!(html.contains("/thumbnails/artifact1234?v=deadbeefcafebabe"));
    assert!(html.contains("markNotificationsSeen"));
}

#[test]
fn settings_renders_management_surfaces_without_trusting_runtime_values() {
    let renderer = AskamaPageRenderer::default();
    let attack = "</script>\"&\u{2029}🎉";
    let org = Organization {
        name: OrgId::from("legacy.example"),
        label: attack.to_owned(),
        color: Some("#123456".to_owned()),
        created_at: None,
        domains: vec![format!("domain-{attack}")],
        emails: vec![format!("person+{attack}@example.test")],
        categories: vec![format!("category-{attack}")],
        key_count: 1,
    };
    let view = SettingsView {
        viewer: Viewer {
            email: Some(EmailAddress::from(format!("admin+{attack}@example.test"))),
            org: Some(OrgId::from("admin")),
            is_admin: true,
        },
        keys: vec![PublisherKeySummary {
            client_id: ClientId::from("publisher"),
            org: org.name.clone(),
            label: attack.to_owned(),
            role: "author".to_owned(),
            owner_email: None,
            created_at: Timestamp("2026-07-21 12:00:00".to_owned()),
            revoked_at: None,
        }],
        organizations: vec![SettingsOrganization {
            organization: org,
            webhooks: vec![WebhookSummary {
                id: WebhookId::from("webhook-1"),
                label: attack.to_owned(),
                events: vec![WebhookEvent::Published, WebhookEvent::Feedback],
                url: format!("https://masked.invalid/{attack}"),
                last_ok_at: None,
                last_error: None,
            }],
        }],
    };

    let html = renderer.settings(&view).expect("render settings");
    assert!(!html.contains("</script>\"&"));
    assert!(html.contains("&#60;/script&#62;"));
    assert!(html.contains("Specific emails"));
    assert!(html.contains("Cloudflare Access Allow policy"));
    assert!(html.contains("Legacy domain-shaped organization"));
    assert!(html.contains("data-event=\"published\""));
    assert!(html.contains("function saveColor"));
    assert!(html.contains("data-ui=\"app-frame\""));
    assert!(html.contains("data-ui=\"nav-artifacts\""));
    assert!(html.contains("data-ui=\"nav-administration\""));
    assert!(html.contains("aria-current=\"page\""));
    assert!(html.contains("class=\"key-edit\""));
    assert!(html.contains("data-owner=\"\""));
    assert!(html.contains("Save changes"));
    assert!(!html.contains("<span>Gallery</span>"));
}

#[test]
fn viewer_shell_uses_the_single_js_encoder_and_exact_opaque_origin_sandbox() {
    let renderer = AskamaPageRenderer::default();
    let attack = "</script>\"'&<>\u{2028}\u{2029}🎉";
    let viewer = Viewer {
        email: Some(EmailAddress::from(attack)),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    };
    let meta = ArtifactMeta {
        id: ArtifactId::from("artifact1234"),
        client_id: ClientId::from("publisher"),
        org: OrgId::from("acme"),
        title: attack.to_owned(),
        description: attack.to_owned(),
        bytes: 42,
        created_at: Timestamp("2026-07-21 12:00:00".to_owned()),
        updated_at: Timestamp("2026-07-21 12:00:00".to_owned()),
        uploader_label: attack.to_owned(),
        owner_email: Some(attack.to_owned()),
        is_bundle: false,
        entry: String::new(),
        revision: 3,
        category: attack.to_owned(),
        hidden: false,
        body_sha256: "deadbeefcafebabe".to_owned(),
    };
    let authorized =
        AccessPolicy::authorize_viewer(&viewer, Some(meta)).expect("same-org authorization");
    let feedback = Feedback {
        id: FeedbackId::from("feedback-1"),
        artifact_id: ArtifactId::from("artifact1234"),
        org: OrgId::from("acme"),
        parent_id: None,
        viewer_email: Some(EmailAddress::from(attack)),
        author: FeedbackAuthor::Artifact {
            viewer_email: EmailAddress::from(attack),
        },
        body: attack.to_owned(),
        artifact_revision: 3,
        anchor_path: Some(attack.to_owned()),
        anchor_x: Some(0.1),
        anchor_y: Some(0.2),
        anchor_w: Some(0.3),
        anchor_h: Some(0.4),
        anchor_approx: false,
        anchor_page: Some(attack.to_owned()),
        anchor_kind: None,
        anchor_node_id: None,
        anchor_quote: None,
        anchor_version: 1,
        created_at: Timestamp("2026-07-21 12:00:00".to_owned()),
        resolved_at: None,
        resolved_by: None,
        external_created_at: None,
        external_edited_at: None,
        external_deleted_at: None,
    };
    let view = ShellView {
        artifact: authorized,
        navigation: ArtifactNavigation {
            previous_id: Some(ArtifactId::from("newer123456")),
            next_id: Some(ArtifactId::from("older123456")),
            index: 2,
            total: 4,
        },
        reaction: Reaction {
            favorite: 1,
            vote: -1,
        },
        feedback: vec![feedback],
        view_counts: ViewCounts {
            views: 5,
            unique_viewers: 2,
            last_viewed_at: None,
        },
        viewers: None,
        viewer,
        org_accent: Some("#123456".to_owned()),
    };

    assert_eq!(
        js_literal("<>&\u{2028}\u{2029}\"🎉"),
        "\"\\u003c\\u003e\\u0026\\u2028\\u2029\\\"🎉\""
    );
    let html = renderer.shell(&view).expect("render viewer shell");
    assert!(html.contains("sandbox=\"allow-scripts allow-popups allow-forms allow-modals\""));
    assert!(!html.contains("allow-same-origin"));
    assert!(!html.contains("</script>\"'&<>"));
    assert!(html.contains("\\u003c/script\\u003e"));
    assert!(html.contains("anchor:repaint"));
    assert!(html.contains("type!=='anchor:ready'&&data.type!=='anchor:picked'&&data.type!=='anchor:positions'&&data.type!=='anchor:navigate'"));
    assert!(html.contains("url.protocol==='http:'||url.protocol==='https:'"));
    assert!(html.contains("outboundHost.textContent=url.host"));
    assert!(html.contains("window.open(url.href,'_blank','noopener')"));
    let outbound_broker = &html[html
        .find("function parseOutboundHref")
        .expect("shell contains outbound-link broker")
        ..html
            .find("window.addEventListener('message'")
            .expect("broker runs before iframe message handler")];
    assert!(!outbound_broker.contains("innerHTML"));
    assert!(html.contains("data-thread-id=\"feedback-1\""));
    assert!(html.contains("id=\"vdelete-trigger\""));
    assert!(html.contains("id=\"delete-dialog\""));
    assert!(html.contains("id=\"vtitle-toggle\""));
    assert!(html.contains("id=\"vtitle-menu\" role=\"menu\""));
    assert!(html.contains("id=\"vcomment-toggle\""));
    assert!(html.contains("id=\"vshare-toggle\""));
    assert!(html.contains("id=\"vmore-menu\" role=\"menu\""));
    assert!(html.contains("aria-label=\"Back to artifact library\""));
    assert!(html.contains("role=\"menuitemcheckbox\""));
    assert!(html.contains("aria-checked=\"true\""));
}

#[test]
fn fixed_clock_rendering_snapshot_matches_the_real_node_oracle() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let attack = "</script>\"&\u{2028}\u{2029}🎉";
    let input = json!({
        "nowMs": 0,
        "attack": attack,
        "viewer": { "email": attack, "org": "acme🎉", "isAdmin": false },
        "admin": { "email": attack, "org": "admin", "isAdmin": true },
        "meta": {
            "id": "artifact1234", "client_id": "publisher", "org": "acme🎉",
            "title": attack, "description": attack, "bytes": 1310720,
            "created_at": "1970-01-01 00:00:00", "updated_at": "1970-01-01 00:00:00",
            "uploader_label": "", "is_bundle": 0, "entry": "", "revision": 3,
            "category": "Reports", "hidden": 0, "body_sha256": "deadbeefcafebabe"
        },
        "nav": { "prevId": "newer123456", "nextId": "older123456", "index": 2, "total": 4 },
        "reaction": { "favorite": 1, "vote": -1 },
        "feedback": [{
            "id": "feedback-1", "artifact_id": "artifact1234", "org": "acme🎉",
            "parent_id": null, "viewer_email": attack, "body": attack,
            "artifact_revision": 3, "anchor_path": attack, "anchor_x": 0.1,
            "anchor_y": 0.2, "anchor_w": 0.3, "anchor_h": 0.4,
            "anchor_approx": false, "anchor_page": attack,
            "created_at": "1970-01-01 00:00:00", "resolved_at": null
        }],
        "analytics": { "counts": { "views": 5, "unique_viewers": 2 }, "viewers": null },
        "orgAccent": null,
        "sections": [{ "org": "empty", "items": [] }, { "org": "acme🎉", "items": [
            {
                "id": "artifact1234", "client_id": "publisher", "org": "acme🎉",
                "title": attack, "description": attack, "bytes": 1310720,
                "created_at": "1970-01-01 00:00:00", "updated_at": "1970-01-01 00:00:00",
                "uploader_label": "", "is_bundle": 0, "entry": "", "revision": 3,
                "category": "Reports", "hidden": 0, "body_sha256": "deadbeefcafebabe"
            },
            {
                "id": "artifact5678", "client_id": "publisher", "org": "acme🎉",
                "title": "Uncategorized", "description": "", "bytes": 1,
                "created_at": "1970-01-01 00:00:00", "updated_at": "1970-01-01 00:00:00",
                "uploader_label": "", "is_bundle": 0, "entry": "", "revision": 1,
                "category": "", "hidden": 0, "body_sha256": ""
            }
        ]}],
        "reactions": [], "sentiment": [], "viewCounts": [], "topViewed": [],
        "orgColors": { "acme🎉": null },
        "notifications": { "unread": 1, "items": [{
            "id": "feedback-1", "artifact_id": "artifact1234", "artifact_title": attack,
            "viewer_email": attack, "body": attack, "created_at": "1970-01-01 00:00:00", "unread": 1
        }]},
        "keys": [{
            "client_id": "publisher", "org": "acme🎉", "label": attack,
            "created_at": "2026-07-21 12:00:00", "revoked_at": null
        }],
        "organizations": [{
            "name": "acme🎉", "label": attack, "color": null, "domains": [],
            "emails": [attack], "categories": ["Reports"], "keyCount": 1, "webhooks": []
        }]
    });
    let node = run_node_render(&root, &input);

    assert_eq!(
        node["portalCss"].as_str().expect("Node portal CSS"),
        include_str!("../../assets/portal.css")
    );
    assert_eq!(
        node["portalScript"].as_str().expect("Node portal script"),
        include_str!("../../assets/portal.js")
    );
    assert_eq!(
        node["shellCss"].as_str().expect("Node shell CSS"),
        include_str!("../../assets/shell.css")
    );
    assert_eq!(
        node["settingsCss"].as_str().expect("Node settings CSS"),
        format!(
            "{}{}",
            include_str!("../../assets/portal.css"),
            include_str!("../../assets/settings.css")
        )
    );
    assert_eq!(
        node["settingsScript"]
            .as_str()
            .expect("Node settings script"),
        include_str!("../../assets/settings.js")
    );
    assert_eq!(
        node["sandbox"],
        "allow-scripts allow-popups allow-forms allow-modals"
    );
    assert_eq!(node["viewerLiteral"], js_literal(attack));
    assert_eq!(
        node["thumbnailSrc"],
        "/thumbnails/artifact1234?v=deadbeefcafebabe"
    );
    assert_eq!(node["galleryHasBytes"], true);
    assert_eq!(node["galleryHasJustNow"], true);
    assert_eq!(node["settingsEscaped"], true);
    assert_eq!(node["categoryFilters"], json!(["Reports", ""]));
    assert_eq!(node["cardOrder"], json!(["artifact1234", "artifact5678"]));
    assert_eq!(
        node["notificationHref"],
        "/artifact1234?feedback=feedback-1"
    );

    let renderer = AskamaPageRenderer::with_fixed_clock(&AppConfig::default(), 0);
    let viewer = Viewer {
        email: Some(EmailAddress::from(attack)),
        org: Some(OrgId::from("acme🎉")),
        is_admin: false,
    };
    let meta = parity_meta(attack);
    let authorized = AccessPolicy::authorize_viewer(&viewer, Some(meta.clone()))
        .expect("same-org parity fixture");
    let feedback = parity_feedback(attack);
    let shell = renderer
        .shell(&ShellView {
            artifact: authorized,
            navigation: ArtifactNavigation {
                previous_id: Some(ArtifactId::from("newer123456")),
                next_id: Some(ArtifactId::from("older123456")),
                index: 2,
                total: 4,
            },
            reaction: Reaction {
                favorite: 1,
                vote: -1,
            },
            feedback: vec![feedback],
            view_counts: ViewCounts {
                views: 5,
                unique_viewers: 2,
                last_viewed_at: None,
            },
            viewers: None,
            viewer: viewer.clone(),
            org_accent: None,
        })
        .expect("Rust shell parity fixture");
    assert_eq!(html_attribute(&shell, "sandbox"), node["sandbox"]);
    assert_eq!(
        decode_html_attribute(&html_attribute(&shell, "src")),
        node["shellSrc"]
    );
    assert_eq!(
        decode_html_attribute(&html_attribute(&shell, "data-viewer-email")),
        node["viewerLiteral"].as_str().expect("Node viewer literal")
    );
    assert_eq!(
        decode_html_attribute(&html_attribute(&shell, "data-feedback")),
        node["feedbackLiteral"]
            .as_str()
            .expect("Node feedback literal")
    );

    let mut uncategorized = parity_meta("Uncategorized");
    uncategorized.id = ArtifactId::from("artifact5678");
    uncategorized.title = "Uncategorized".to_owned();
    uncategorized.description = String::new();
    uncategorized.bytes = 1;
    uncategorized.revision = 1;
    uncategorized.category = String::new();
    uncategorized.body_sha256 = String::new();
    let gallery = renderer
        .gallery(&GalleryView {
            viewer,
            sections: vec![
                OrgArtifacts {
                    org: OrgId::from("empty"),
                    items: Vec::new(),
                },
                OrgArtifacts {
                    org: OrgId::from("acme🎉"),
                    items: vec![meta, uncategorized],
                },
            ],
            reactions: BTreeMap::new(),
            sentiment: BTreeMap::new(),
            view_counts: BTreeMap::new(),
            top_viewed: BTreeMap::new(),
            org_colors: BTreeMap::new(),
            notifications: vec![ViewerNotification {
                id: FeedbackId::from("feedback-1"),
                artifact_id: ArtifactId::from("artifact1234"),
                artifact_title: attack.to_owned(),
                org: OrgId::from("acme🎉"),
                body: attack.to_owned(),
                author: FeedbackAuthor::Artifact {
                    viewer_email: EmailAddress::from(attack),
                },
                created_at: Timestamp("1970-01-01 00:00:00".to_owned()),
                parent_id: None,
                resolved: false,
                has_anchor: false,
                unread: true,
            }],
            unread_notifications: 1,
        })
        .expect("Rust gallery parity fixture");
    assert_eq!(
        html_attribute(&gallery, "src"),
        node["thumbnailSrc"]
            .as_str()
            .expect("Node thumbnail source")
    );
    assert!(gallery.contains("data-filter-category=\"Reports\""));
    assert!(gallery.contains("data-filter-category=\"\""));
    assert_eq!(
        html_attributes_after(&gallery, "<article class=\"card", "data-id"),
        vec!["artifact1234", "artifact5678"]
    );
    assert!(gallery.contains(&format!(
        "--org-k:{};",
        node["orgColor"].as_str().expect("Node organization color")
    )));
    assert_eq!(
        html_attribute_after(&gallery, "<a class=\"notif-row", "href"),
        node["notificationHref"]
    );
}

fn parity_meta(attack: &str) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId::from("artifact1234"),
        client_id: ClientId::from("publisher"),
        org: OrgId::from("acme🎉"),
        title: attack.to_owned(),
        description: attack.to_owned(),
        bytes: 1_310_720,
        created_at: Timestamp("1970-01-01 00:00:00".to_owned()),
        updated_at: Timestamp("1970-01-01 00:00:00".to_owned()),
        uploader_label: String::new(),
        owner_email: None,
        is_bundle: false,
        entry: String::new(),
        revision: 3,
        category: "Reports".to_owned(),
        hidden: false,
        body_sha256: "deadbeefcafebabe".to_owned(),
    }
}

fn parity_feedback(attack: &str) -> Feedback {
    Feedback {
        id: FeedbackId::from("feedback-1"),
        artifact_id: ArtifactId::from("artifact1234"),
        org: OrgId::from("acme🎉"),
        parent_id: None,
        viewer_email: Some(EmailAddress::from(attack)),
        author: FeedbackAuthor::Artifact {
            viewer_email: EmailAddress::from(attack),
        },
        body: attack.to_owned(),
        artifact_revision: 3,
        anchor_path: Some(attack.to_owned()),
        anchor_x: Some(0.1),
        anchor_y: Some(0.2),
        anchor_w: Some(0.3),
        anchor_h: Some(0.4),
        anchor_approx: false,
        anchor_page: Some(attack.to_owned()),
        anchor_kind: None,
        anchor_node_id: None,
        anchor_quote: None,
        anchor_version: 1,
        created_at: Timestamp("1970-01-01 00:00:00".to_owned()),
        resolved_at: None,
        resolved_by: None,
        external_created_at: None,
        external_edited_at: None,
        external_deleted_at: None,
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn node_reference_available(root: &Path) -> bool {
    let unavailable =
        if root.join("lib/portal.js").is_file() && root.join("lib/settings.js").is_file() {
            match Command::new("node").arg("--version").output() {
                Ok(output) if output.status.success() => None,
                _ => Some("node is not on PATH"),
            }
        } else {
            Some("lib/portal.js or lib/settings.js is missing")
        };
    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                std::env::var(REQUIRE_NODE_REFERENCE).ok().as_deref() != Some("1"),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node renderer is unavailable ({reason})"
            );
            eprintln!("skipping U15 Node rendering parity proof: {reason}");
            false
        }
    }
}

fn run_node_render(root: &Path, input: &Value) -> Value {
    let portal = format!("file://{}", root.join("lib/portal.js").display());
    let settings = format!("file://{}", root.join("lib/settings.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_RENDER_DRIVER)
        .arg(portal)
        .arg(settings)
        .arg(input.to_string())
        .env("PUBLIC_BASE_URL", "http://localhost:3480")
        .env("APP_NAME", "Artifact Index")
        .env("APP_BRAND", "A")
        .output()
        .expect("run Node renderer oracle");
    assert!(
        output.status.success(),
        "Node renderer failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Node renderer emitted JSON")
}

fn html_attribute(html: &str, name: &str) -> String {
    let marker = format!("{name}=\"");
    let start = html.find(&marker).expect("attribute exists") + marker.len();
    let tail = &html[start..];
    tail[..tail.find('"').expect("attribute closes")].to_owned()
}

fn decode_html_attribute(value: &str) -> String {
    value
        .replace("&#34;", "\"")
        .replace("&#39;", "'")
        .replace("&#60;", "<")
        .replace("&#62;", ">")
        .replace("&#38;", "&")
}

fn html_attribute_after(html: &str, after: &str, name: &str) -> String {
    let start = html.find(after).expect("anchor exists");
    html_attribute(&html[start..], name)
}

fn html_attributes_after(html: &str, after: &str, name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut remaining = html;
    while let Some(start) = remaining.find(after) {
        remaining = &remaining[start + after.len()..];
        values.push(html_attribute(remaining, name));
    }
    values
}
