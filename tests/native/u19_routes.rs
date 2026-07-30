use std::{
    collections::BTreeMap,
    path::{Path as FsPath, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use artifact_mcp::{
    AppDeps, build_router,
    config::AppConfig,
    error::AppError,
    mcp::{
        dispatch::{ProtocolEra, dispatch, dispatch_for_era},
        protocol::OrderedJson,
    },
    model::*,
    ports::{
        AdminService, ArtifactService, BoxFuture, EngagementService, HealthProbe, NotificationSink,
        PageRenderer, PreviewService, PublisherAuthenticator, ShareService, ViewerIdentity,
        integrations::{HealthReport, PreviewPriority},
    },
    render::view_models::{GalleryView, SettingsView, ShellView},
    security::access::{AuthorizedArtifact, OwnedArtifact},
};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
};
use tower::ServiceExt;

const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const NODE_ROUTE_DRIVER: &str = r#"
import(process.argv[1]).then(async ({ createApp }) => {
  const meta = {
    id: "abc123def456", client_id: "publisher", org: "acme", title: "Artifact",
    description: "Description", bytes: 42, created_at: "2026-07-20T00:00:00.000Z",
    updated_at: "2026-07-20T00:00:00.000Z", uploader_label: "Publisher",
    is_bundle: 0, entry: "", revision: 2, category: "Reports", hidden: 0,
    body_sha256: "0".repeat(64)
  };
  const revision = {
    artifact_id: meta.id, org: meta.org, revision: 1, title: "Earlier",
    description: "Earlier description", category: "Archive", bytes: 21,
    is_bundle: 0, entry: "", body_sha256: "1".repeat(64),
    created_at: "2026-07-19T00:00:00.000Z"
  };
  const feedbackRow = {
    id: "feedback-id", artifact_id: meta.id, org: meta.org,
    viewer_email: "viewer@example.test", body: "Note", artifact_revision: 2,
    created_at: "2026-07-21 00:00:00", resolved_at: null, resolved_by: null,
    parent_id: null, anchor_path: null, anchor_x: null, anchor_y: null,
    anchor_w: null, anchor_h: null, anchor_approx: 0, anchor_page: null
  };
  let viewer = { email: "viewer@example.test", org: "acme", isAdmin: false };
  const artifacts = {
    isReserved: () => false,
    getArtifactMeta: () => meta,
    deleteArtifactById: () => true,
    setCategory: (_id, category) => ({ ok: true, id: meta.id, category: String(category || "") }),
    setHidden: (_id, hidden) => ({ ok: true, id: meta.id, hidden: Boolean(hidden) }),
    moveArtifactToOrg: (_id, org, category) => ({ ok: true, id: meta.id, org, category }),
    listRevisions: () => ({ current: 2, revisions: [revision] }),
    restoreArtifactRevision: (_id, restoredFrom) => ({ ok: true, revision: 3, restoredFrom, bytes: 42 })
  };
  const shares = {
    create: () => ({ token: "share-token", expires_at: null }),
    listForArtifact: () => [{
      token: "share-token", expires_at: null, created_at: "2026-07-20 00:00:00",
      created_by: "viewer@example.test"
    }],
    revoke: () => true,
    resolve: () => null
  };
  const feedback = {
    listForArtifact: () => [feedbackRow],
    add: () => feedbackRow,
    getFeedback: () => feedbackRow,
    deleteFeedback: (id) => ({ ok: true, id }),
    resolveByViewer: (id) => ({ ok: true, id, changed: true })
  };
  const app = createApp({
    checkPublisherKey: () => ({ ok: false }),
    handleMcp: async () => null,
    resolveViewer: async () => viewer,
    artifacts,
    shares,
    keys: {},
    orgs: {},
    webhooks: {},
    notify: { emit() {}, test: async () => ({ ok: false }) },
    reactions: { set: () => ({ favorite: 0, vote: -1 }) },
    feedback,
    thumbnails: { removeArtifact: async () => {} },
    pages: { notFound: () => "Not found" },
    logger: {},
    publicBase: "http://localhost:3480"
  });

  async function invoke(routePath, method, params, body, nextViewer = viewer) {
    viewer = nextViewer;
    const layer = app._router.stack.find((candidate) =>
      candidate.route?.path === routePath && candidate.route.methods[method]
    );
    if (!layer) throw new Error(`missing route ${method} ${routePath}`);
    const handler = layer.route.stack[layer.route.stack.length - 1].handle;
    const response = {
      statusCode: 200,
      headers: {},
      body: null,
      status(code) { this.statusCode = code; return this; },
      set(name, value) {
        if (typeof name === "object") {
          for (const [key, item] of Object.entries(name)) this.headers[key.toLowerCase()] = String(item);
        } else {
          this.headers[String(name).toLowerCase()] = String(value);
        }
        return this;
      },
      json(value) { this.body = value; return this; },
      send(value) { this.body = value; return this; },
      end() { return this; }
    };
    await handler({ params, body }, response);
    return { status: response.statusCode, body: response.body };
  }

  const signed = { email: "viewer@example.test", org: "acme", isAdmin: false };
  const cross = { email: "viewer@example.test", org: "beta", isAdmin: false };
  const out = {};
  out.delete = await invoke("/:id", "delete", { id: meta.id }, undefined, signed);
  out.react = await invoke("/:id/react", "post", { id: meta.id }, { favorite: 0, vote: -1 }, signed);
  out.category = await invoke("/:id/category", "post", { id: meta.id }, { category: "Dashboards" }, signed);
  out.visibility = await invoke("/:id/visibility", "post", { id: meta.id }, { hidden: true }, signed);
  out.moveCrossOrg = await invoke("/:id/move", "post", { id: meta.id }, { org: "beta" }, cross);
  out.moveSameOrg = await invoke("/:id/move", "post", { id: meta.id }, { org: "beta" }, signed);
  out.shareCreate = await invoke("/:id/share", "post", { id: meta.id }, { expires: "never" }, signed);
  out.shareList = await invoke("/:id/shares", "get", { id: meta.id }, undefined, signed);
  out.shareRevoke = await invoke("/:id/shares/:token", "delete", { id: meta.id, token: "share-token" }, undefined, signed);
  out.feedbackList = await invoke("/:id/feedback", "get", { id: meta.id }, undefined, signed);
  out.feedbackCreate = await invoke("/:id/feedback", "post", { id: meta.id }, { body: " Note ", anchor: null }, signed);
  out.feedbackDelete = await invoke("/:id/feedback/:fid", "delete", { id: meta.id, fid: "feedback-id" }, undefined, signed);
  out.feedbackResolve = await invoke("/:id/feedback/:fid/resolve", "post", { id: meta.id, fid: "feedback-id" }, undefined, signed);
  out.history = await invoke("/:id/history", "get", { id: meta.id }, undefined, signed);
  out.restore = await invoke("/:id/restore", "post", { id: meta.id }, { revision: "1" }, signed);
  process.stdout.write(JSON.stringify(out));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

fn unavailable<'a, T>() -> BoxFuture<'a, Result<T, AppError>> {
    Box::pin(async {
        Err(AppError::Unavailable(
            "unused U19 test capability".to_owned(),
        ))
    })
}

#[derive(Clone)]
struct RouteHarness {
    viewer: Viewer,
    meta: Option<ArtifactMeta>,
    publisher: Option<PublisherIdentity>,
    calls: Arc<Mutex<Vec<String>>>,
    fail_category_registration: Arc<Mutex<bool>>,
    reaction_update: Arc<Mutex<Option<ReactionUpdate>>>,
    feedback_submission: Arc<Mutex<Option<SubmitFeedback>>>,
    notifications: Arc<Mutex<Vec<(WebhookEvent, OrgId, NotificationPayload)>>>,
}

impl RouteHarness {
    fn new(viewer: Viewer, meta: Option<ArtifactMeta>) -> Self {
        Self {
            viewer,
            meta,
            publisher: None,
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_category_registration: Arc::new(Mutex::new(false)),
            reaction_update: Arc::new(Mutex::new(None)),
            feedback_submission: Arc::new(Mutex::new(None)),
            notifications: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, call: &str) {
        self.calls.lock().expect("calls lock").push(call.to_owned());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }

    fn fail_category_registration(&self) {
        *self
            .fail_category_registration
            .lock()
            .expect("category registration lock") = true;
    }

    fn with_publisher(mut self, publisher: PublisherIdentity) -> Self {
        self.publisher = Some(publisher);
        self
    }

    fn reaction_update(&self) -> Option<ReactionUpdate> {
        *self.reaction_update.lock().expect("reaction lock")
    }

    fn feedback_submission(&self) -> Option<SubmitFeedback> {
        self.feedback_submission
            .lock()
            .expect("feedback lock")
            .clone()
    }

    fn notifications(&self) -> Vec<(WebhookEvent, OrgId, NotificationPayload)> {
        self.notifications
            .lock()
            .expect("notifications lock")
            .clone()
    }
}

impl PublisherAuthenticator for RouteHarness {
    fn authenticate<'a>(
        &'a self,
        _headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>> {
        let publisher = self.publisher.clone();
        Box::pin(async move {
            publisher.ok_or_else(|| {
                AppError::Unauthorized("publisher authentication required".to_owned())
            })
        })
    }
}

impl ViewerIdentity for RouteHarness {
    fn resolve<'a>(&'a self, _headers: &'a HeaderMap) -> BoxFuture<'a, Result<Viewer, AppError>> {
        self.record("resolve_viewer");
        let viewer = self.viewer.clone();
        Box::pin(async move { Ok(viewer) })
    }
}

impl ArtifactService for RouteHarness {
    fn find_meta<'a>(
        &'a self,
        _id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        self.record("find_meta");
        let meta = self.meta.clone();
        Box::pin(async move { Ok(meta) })
    }

    fn publish(
        &self,
        _request: PublishArtifact,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>> {
        unavailable()
    }

    fn list_for_publisher<'a>(
        &'a self,
        _publisher: &'a PublisherIdentity,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        unavailable()
    }

    fn list_org_artifacts<'a>(
        &'a self,
        _org: &'a OrgId,
        _include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        unavailable()
    }

    fn list_all_grouped_by_org(
        &self,
        _include_hidden: bool,
    ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>> {
        unavailable()
    }

    fn list_org_ids<'a>(
        &'a self,
        _org: &'a OrgId,
        _include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>> {
        unavailable()
    }

    fn read_body<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        self.record("read_body");
        unavailable()
    }

    fn read_bundle_file<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _relative_path: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        self.record("read_bundle_file");
        unavailable()
    }

    fn read_revision_body<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _revision: u64,
        _relative_path: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        self.record("read_revision_body");
        unavailable()
    }

    fn list_bundle_files<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _revision: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Vec<(String, u64)>>, AppError>> {
        self.record("list_bundle_files");
        unavailable()
    }

    fn list_revisions<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<RevisionHistory, AppError>> {
        self.record("list_revisions");
        let meta = artifact.meta().clone();
        Box::pin(async move {
            Ok(RevisionHistory {
                current: meta.revision,
                revisions: vec![ArtifactRevision {
                    artifact_id: meta.id,
                    org: meta.org,
                    revision: 1,
                    title: "Earlier".to_owned(),
                    description: "Earlier description".to_owned(),
                    category: "Archive".to_owned(),
                    bytes: 21,
                    is_bundle: false,
                    entry: String::new(),
                    body_sha256: "1".repeat(64),
                    created_at: Timestamp("2026-07-19T00:00:00.000Z".to_owned()),
                    client_id: None,
                }],
            })
        })
    }

    fn update(
        &self,
        _artifact: AuthorizedArtifact,
        _update: ArtifactUpdate,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
        unavailable()
    }

    fn restore(
        &self,
        artifact: AuthorizedArtifact,
        revision: u64,
        _acting_client_id: Option<ClientId>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
        self.record("restore");
        let mut meta = artifact.into_meta();
        meta.revision += 1;
        Box::pin(async move {
            Ok(RestoreArtifactResult {
                meta,
                restored_from: revision,
            })
        })
    }

    fn delete(
        &self,
        _artifact: AuthorizedArtifact,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        self.record("delete");
        Box::pin(async { Ok(true) })
    }

    fn set_category(
        &self,
        _artifact: AuthorizedArtifact,
        category: String,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        self.record("set_category");
        let mut meta = self.meta.clone().expect("configured meta");
        meta.category = category;
        Box::pin(async move { Ok(meta) })
    }

    fn set_hidden(
        &self,
        _artifact: AuthorizedArtifact,
        hidden: bool,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        self.record("set_hidden");
        let mut meta = self.meta.clone().expect("configured meta");
        meta.hidden = hidden;
        Box::pin(async move { Ok(meta) })
    }

    fn move_to_org(
        &self,
        artifact: AuthorizedArtifact,
        target_org: OrgId,
        category: Option<String>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        self.record("move_to_org");
        let mut meta = artifact.into_meta();
        meta.org = target_org;
        if let Some(category) = category {
            meta.category = category;
        }
        Box::pin(async move { Ok(meta) })
    }

    fn audit_storage(
        &self,
        _clean_transient: bool,
    ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>> {
        unavailable()
    }

    fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>> {
        unavailable()
    }
}

impl AdminService for RouteHarness {
    fn list_keys(&self) -> BoxFuture<'_, Result<Vec<PublisherKeySummary>, AppError>> {
        unavailable()
    }
    fn create_key(
        &self,
        _request: CreatePublisherKey,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<CreatedPublisherKey, AppError>> {
        unavailable()
    }
    fn revoke_key<'a>(
        &'a self,
        _client_id: &'a ClientId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unavailable()
    }
    fn org_exists<'a>(&'a self, _org: &'a OrgId) -> BoxFuture<'a, Result<bool, AppError>> {
        unavailable()
    }
    fn org_for_domain<'a>(
        &'a self,
        _domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        unavailable()
    }
    fn org_for_email<'a>(
        &'a self,
        _email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        unavailable()
    }
    fn org_names(&self) -> BoxFuture<'_, Result<Vec<OrgId>, AppError>> {
        unavailable()
    }
    fn list_orgs(&self) -> BoxFuture<'_, Result<Vec<Organization>, AppError>> {
        unavailable()
    }
    fn create_org(
        &self,
        _request: CreateOrganization,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<Organization, AppError>> {
        unavailable()
    }
    fn delete_org<'a>(
        &'a self,
        _org: &'a OrgId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unavailable()
    }
    fn add_domain<'a>(
        &'a self,
        _org: &'a OrgId,
        _domain: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        unavailable()
    }
    fn remove_domain<'a>(
        &'a self,
        _org: &'a OrgId,
        _domain: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unavailable()
    }
    fn add_email_member<'a>(
        &'a self,
        _org: &'a OrgId,
        _email: &'a EmailAddress,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<EmailAddress, AppError>> {
        unavailable()
    }
    fn remove_email_member<'a>(
        &'a self,
        _org: &'a OrgId,
        _email: &'a EmailAddress,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unavailable()
    }
    fn categories<'a>(&'a self, _org: &'a OrgId) -> BoxFuture<'a, Result<Vec<String>, AppError>> {
        unavailable()
    }
    fn add_category<'a>(
        &'a self,
        org: &'a OrgId,
        name: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        // Records org and name so a test can prove the category route registers the category on
        // the org (the fix for the Settings-picker bug).
        self.record(&format!("add_category({},{name})", org.0));
        if *self
            .fail_category_registration
            .lock()
            .expect("category registration lock")
        {
            return Box::pin(async { Err(AppError::Internal) });
        }
        let name = name.to_owned();
        Box::pin(async move { Ok(name) })
    }
    fn remove_category<'a>(
        &'a self,
        _org: &'a OrgId,
        _name: &'a str,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unavailable()
    }
    fn color_map(&self) -> BoxFuture<'_, Result<BTreeMap<OrgId, Option<String>>, AppError>> {
        unavailable()
    }
    fn set_color<'a>(
        &'a self,
        _org: &'a OrgId,
        _color: Option<&'a str>,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        unavailable()
    }
    fn list_webhooks<'a>(
        &'a self,
        _org: &'a OrgId,
    ) -> BoxFuture<'a, Result<Vec<WebhookSummary>, AppError>> {
        unavailable()
    }
    fn create_webhook(
        &self,
        _request: CreateWebhook,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<WebhookSummary, AppError>> {
        unavailable()
    }
    fn remove_webhook<'a>(
        &'a self,
        _org: &'a OrgId,
        _id: &'a WebhookId,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        unavailable()
    }
    fn set_webhook_events<'a>(
        &'a self,
        _org: &'a OrgId,
        _id: &'a WebhookId,
        _events: &'a [WebhookEvent],
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'a, Result<Option<WebhookSummary>, AppError>> {
        unavailable()
    }
    fn webhook_delivery<'a>(
        &'a self,
        _id: &'a WebhookId,
    ) -> BoxFuture<'a, Result<Option<WebhookDelivery>, AppError>> {
        unavailable()
    }
}

impl EngagementService for RouteHarness {
    fn reaction<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<Reaction, AppError>> {
        unavailable()
    }
    fn set_reaction(
        &self,
        _artifact: AuthorizedArtifact,
        _viewer: Viewer,
        update: ReactionUpdate,
    ) -> BoxFuture<'_, Result<Reaction, AppError>> {
        self.record("set_reaction");
        *self.reaction_update.lock().expect("reaction lock") = Some(update);
        Box::pin(async {
            Ok(Reaction {
                favorite: 0,
                vote: -1,
            })
        })
    }
    fn reactions_for_viewer<'a>(
        &'a self,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, Reaction>, AppError>> {
        unavailable()
    }
    fn sentiment(&self) -> BoxFuture<'_, Result<BTreeMap<ArtifactId, Sentiment>, AppError>> {
        unavailable()
    }
    fn record_view<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        unavailable()
    }
    fn view_counts<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<ViewCounts, AppError>> {
        unavailable()
    }
    fn view_counts_for_org<'a>(
        &'a self,
        _org: &'a OrgId,
    ) -> BoxFuture<'a, Result<BTreeMap<ArtifactId, ViewCounts>, AppError>> {
        unavailable()
    }
    fn viewers<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<ViewerView>, AppError>> {
        unavailable()
    }
    fn top_for_org<'a>(
        &'a self,
        _org: &'a OrgId,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TopViewedArtifact>, AppError>> {
        unavailable()
    }
    fn feedback_ref<'a>(
        &'a self,
        id: &'a FeedbackId,
    ) -> BoxFuture<'a, Result<Option<FeedbackRef>, AppError>> {
        self.record("feedback_ref");
        let reference = self.meta.as_ref().map(|meta| FeedbackRef {
            id: id.clone(),
            artifact_id: meta.id.clone(),
            org: meta.org.clone(),
        });
        Box::pin(async move { Ok(reference) })
    }
    fn list_feedback<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        self.record("list_feedback");
        let row = feedback_row(artifact.meta());
        Box::pin(async move { Ok(vec![row]) })
    }
    fn submit_feedback(
        &self,
        artifact: AuthorizedArtifact,
        submission: SubmitFeedback,
    ) -> BoxFuture<'_, Result<Feedback, AppError>> {
        self.record("submit_feedback");
        *self.feedback_submission.lock().expect("feedback lock") = Some(submission);
        let row = feedback_row(artifact.meta());
        Box::pin(async move { Ok(row) })
    }
    fn delete_feedback(
        &self,
        _artifact: AuthorizedArtifact,
        _viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        self.record("delete_feedback");
        Box::pin(async move { Ok(FeedbackMutation { id, changed: true }) })
    }
    fn resolve_feedback_as_viewer(
        &self,
        _artifact: AuthorizedArtifact,
        _viewer: Viewer,
        id: FeedbackId,
    ) -> BoxFuture<'_, Result<FeedbackMutation, AppError>> {
        self.record("resolve_feedback_as_viewer");
        Box::pin(async move { Ok(FeedbackMutation { id, changed: true }) })
    }
    fn list_feedback_for_publisher<'a>(
        &'a self,
        _publisher: &'a PublisherIdentity,
        _artifact: Option<&'a OwnedArtifact>,
    ) -> BoxFuture<'a, Result<Vec<Feedback>, AppError>> {
        unavailable()
    }
    fn resolve_feedback_as_publisher(
        &self,
        _artifact: OwnedArtifact,
        _id: FeedbackId,
        _resolved_by: String,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        self.record("resolve_feedback_as_publisher");
        Box::pin(async { Ok(true) })
    }
    fn reopen_feedback_as_publisher(
        &self,
        _artifact: OwnedArtifact,
        _id: FeedbackId,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        unavailable()
    }
    fn recent_notifications<'a>(
        &'a self,
        _viewer: &'a Viewer,
        _limit: usize,
    ) -> BoxFuture<'a, Result<Vec<ViewerNotification>, AppError>> {
        unavailable()
    }
    fn unread_notifications<'a>(
        &'a self,
        _viewer: &'a Viewer,
    ) -> BoxFuture<'a, Result<u64, AppError>> {
        unavailable()
    }
    fn mark_notifications_seen<'a>(
        &'a self,
        _email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        unavailable()
    }
}

impl ShareService for RouteHarness {
    fn resolve<'a>(
        &'a self,
        _token: &'a ShareToken,
    ) -> BoxFuture<'a, Result<Option<ShareGrant>, AppError>> {
        unavailable()
    }
    fn create(
        &self,
        _artifact: AuthorizedArtifact,
        _request: CreateShare,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<PublicShare, AppError>> {
        self.record("create_share");
        Box::pin(async {
            Ok(PublicShare {
                token: ShareToken::from("share-token"),
                expires_at: None,
                created_at: None,
                created_by: None,
            })
        })
    }
    fn list<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Vec<PublicShare>, AppError>> {
        self.record("list_shares");
        Box::pin(async {
            Ok(vec![PublicShare {
                token: ShareToken::from("share-token"),
                expires_at: None,
                created_at: Some(Timestamp("2026-07-20 00:00:00".to_owned())),
                created_by: Some("viewer@example.test".to_owned()),
            }])
        })
    }
    fn revoke(
        &self,
        _artifact: AuthorizedArtifact,
        _token: ShareToken,
        _audit: artifact_mcp::security::audit::MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        self.record("revoke_share");
        Box::pin(async { Ok(true) })
    }
}

impl PageRenderer for RouteHarness {
    fn gallery(&self, _view: &GalleryView) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused".to_owned()))
    }
    fn shell(&self, _view: &ShellView) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused".to_owned()))
    }
    fn settings(&self, _view: &SettingsView) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused".to_owned()))
    }
    fn not_found(&self, _message: Option<&str>) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused".to_owned()))
    }
    fn not_signed_in(&self) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused".to_owned()))
    }
    fn access_retry(&self, _target: &str) -> Result<String, AppError> {
        Err(AppError::Unavailable("unused".to_owned()))
    }
}

impl PreviewService for RouteHarness {
    fn enabled(&self) -> bool {
        false
    }
    fn read_thumbnail<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _digest: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        unavailable()
    }
    fn placeholder(&self, _meta: &ArtifactMeta, _accent: Option<&str>) -> Vec<u8> {
        Vec::new()
    }
    fn ensure_thumbnail<'a>(
        &'a self,
        _meta: &'a ArtifactMeta,
        _html: &'a str,
        _priority: PreviewPriority,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        unavailable()
    }
    fn remove_artifact<'a>(&'a self, _id: &'a ArtifactId) -> BoxFuture<'a, Result<(), AppError>> {
        self.record("remove_preview");
        Box::pin(async { Ok(()) })
    }
}

impl NotificationSink for RouteHarness {
    fn emit(
        &self,
        event: WebhookEvent,
        org: OrgId,
        payload: NotificationPayload,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        self.record("notify");
        self.notifications
            .lock()
            .expect("notifications lock")
            .push((event, org, payload));
        Box::pin(async { Ok(()) })
    }
    fn test<'a>(
        &'a self,
        _webhook: &'a WebhookDelivery,
    ) -> BoxFuture<'a, Result<DeliveryResult, AppError>> {
        unavailable()
    }
}

impl HealthProbe for RouteHarness {
    fn check(&self) -> BoxFuture<'_, Result<HealthReport, AppError>> {
        Box::pin(async { Ok(HealthReport::ok()) })
    }
}

fn meta(org: &str) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId::from("abc123def456"),
        client_id: ClientId::from("publisher"),
        org: OrgId::from(org),
        title: "Artifact".to_owned(),
        description: "Description".to_owned(),
        bytes: 42,
        created_at: Timestamp("2026-07-20T00:00:00.000Z".to_owned()),
        updated_at: Timestamp("2026-07-20T00:00:00.000Z".to_owned()),
        uploader_label: "Publisher".to_owned(),
        owner_email: None,
        is_bundle: false,
        entry: String::new(),
        revision: 2,
        category: "Reports".to_owned(),
        hidden: false,
        body_sha256: "0".repeat(64),
    }
}

fn viewer(org: &str, is_admin: bool) -> Viewer {
    Viewer {
        email: Some(EmailAddress::from("viewer@example.test")),
        org: Some(OrgId::from(org)),
        is_admin,
    }
}

fn feedback_row(meta: &ArtifactMeta) -> Feedback {
    Feedback {
        id: FeedbackId::from("feedback-id"),
        artifact_id: meta.id.clone(),
        org: meta.org.clone(),
        parent_id: None,
        viewer_email: Some(EmailAddress::from("viewer@example.test")),
        author: FeedbackAuthor::Artifact {
            viewer_email: EmailAddress::from("viewer@example.test"),
        },
        body: "Note".to_owned(),
        artifact_revision: meta.revision,
        anchor_path: None,
        anchor_x: None,
        anchor_y: None,
        anchor_w: None,
        anchor_h: None,
        anchor_approx: false,
        anchor_page: None,
        created_at: Timestamp("2026-07-21 00:00:00".to_owned()),
        resolved_at: None,
        resolved_by: None,
        external_created_at: None,
        external_edited_at: None,
        external_deleted_at: None,
    }
}

fn deps(harness: Arc<RouteHarness>) -> AppDeps {
    AppDeps {
        publisher_auth: harness.clone(),
        viewer_identity: harness.clone(),
        artifacts: harness.clone(),
        admin: harness.clone(),
        discussions: Arc::new(artifact_mcp::ports::InertDiscussionService),
        engagement: harness.clone(),
        shares: harness.clone(),
        pages: harness.clone(),
        previews: harness.clone(),
        notifications: harness.clone(),
        health: harness,
        ingress: Arc::new(artifact_mcp::http::ingress::IngressState::from_config(
            &AppConfig::default(),
        )),
        preview_tasks: artifact_mcp::mcp::tasks::PreviewTaskStore::new(
            std::env::temp_dir().join(format!("artifact-mcp-u19-tasks-{}", std::process::id())),
        ),
        mcp_telemetry: artifact_mcp::observability::McpTelemetry::default(),
        delivery_telemetry:
            artifact_mcp::integrations::delivery_runtime::DeliveryTelemetry::default(),
        delivery_wake: artifact_mcp::integrations::delivery_runtime::DeliveryWakeSignal::default(),
        audit_access: None,
        config: Arc::new(AppConfig::default()),
    }
}

async fn body(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("response body")
        .to_vec()
}

#[tokio::test]
async fn cross_org_delete_is_concealed_before_the_delete_service() {
    let harness = Arc::new(RouteHarness::new(viewer("beta", false), Some(meta("acme"))));
    let response = build_router(deps(harness.clone()))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/abc123def456")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(response).await, br#"{"error":"Not found"}"#);
    assert_eq!(harness.calls(), ["resolve_viewer", "find_meta"]);
}

#[tokio::test]
async fn same_org_non_owners_and_legacy_members_cannot_delete() {
    for owner_email in [Some("owner@example.test".to_owned()), None] {
        let mut artifact = meta("acme");
        artifact.owner_email = owner_email;
        let harness = Arc::new(RouteHarness::new(viewer("acme", false), Some(artifact)));
        let response = build_router(deps(harness.clone()))
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/abc123def456")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(body(response).await, br#"{"error":"Forbidden"}"#);
        assert_eq!(harness.calls(), ["resolve_viewer", "find_meta"]);
    }
}

#[tokio::test]
async fn delete_uses_lifecycle_cleanup_without_detached_notification() {
    let mut owned = meta("acme");
    owned.owner_email = Some("VIEWER@EXAMPLE.TEST".to_owned());
    let harness = Arc::new(RouteHarness::new(viewer("acme", false), Some(owned)));
    let response = build_router(deps(harness.clone()))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/abc123def456")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"id":"abc123def456","deleted":true}"#
    );
    tokio::task::yield_now().await;
    let calls = harness.calls();
    assert_eq!(&calls[..3], ["resolve_viewer", "find_meta", "delete"]);
    assert!(calls[3..].contains(&"remove_preview".to_owned()));
    assert!(!calls[3..].contains(&"notify".to_owned()));
    assert!(harness.notifications().is_empty());
}

#[tokio::test]
async fn administrators_can_delete_legacy_unowned_artifacts() {
    let harness = Arc::new(RouteHarness::new(viewer("admin", true), Some(meta("acme"))));
    let response = build_router(deps(harness.clone()))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/abc123def456")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"id":"abc123def456","deleted":true}"#
    );
    assert_eq!(
        &harness.calls()[..3],
        ["resolve_viewer", "find_meta", "delete"]
    );
}

#[tokio::test]
async fn move_conceals_before_applying_the_admin_policy() {
    let cross_org = Arc::new(RouteHarness::new(viewer("beta", false), Some(meta("acme"))));
    let response = build_router(deps(cross_org.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/move")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"org":"beta"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(response).await, br#"{"error":"Not found"}"#);
    assert_eq!(cross_org.calls(), ["resolve_viewer", "find_meta"]);

    let same_org = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(same_org.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/move")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"org":"beta"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(response).await, br#"{"error":"Admins only"}"#);
    assert_eq!(same_org.calls(), ["resolve_viewer", "find_meta"]);

    let admin = Arc::new(RouteHarness::new(viewer("acme", true), Some(meta("acme"))));
    let response = build_router(deps(admin.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/move")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"org":"beta","category":"Moved"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"id":"abc123def456","org":"beta","category":"Moved"}"#
    );
    // The audited registry write is a precondition for the move, so a registry failure cannot
    // report a move whose category remains invisible in Settings.
    assert_eq!(
        admin.calls(),
        [
            "resolve_viewer",
            "find_meta",
            "add_category(beta,Moved)",
            "move_to_org"
        ]
    );
}

#[tokio::test]
async fn reaction_route_uses_node_input_rules_and_the_engagement_service() {
    let harness = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(harness.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/react")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"favorite":0,"vote":-1}"#))
                .expect("request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, br#"{"favorite":0,"vote":-1}"#);
    assert_eq!(
        harness.reaction_update(),
        Some(ReactionUpdate {
            favorite: Some(false),
            vote: Some(-1),
        })
    );
    assert_eq!(
        harness.calls(),
        ["resolve_viewer", "find_meta", "set_reaction"]
    );
}

#[tokio::test]
async fn action_json_parser_matches_node_errors_before_route_services_run() {
    let malformed = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(malformed.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/react")
                .header("content-type", "application/json")
                .body(Body::from("{"))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body(response).await, br#"{"error":"invalid JSON"}"#);
    assert!(malformed.calls().is_empty());

    let oversized = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let payload = format!(r#"{{"category":"{}"}}"#, "x".repeat(9 * 1024));
    let response = build_router(deps(oversized.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/category")
                .header("content-type", "application/json")
                .body(Body::from(payload))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body(response).await, br#"{"error":"payload too large"}"#);
    assert!(oversized.calls().is_empty());
}

#[tokio::test]
async fn category_and_visibility_routes_return_the_persisted_values() {
    let category = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(category.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/category")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"category":"Dashboards"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"id":"abc123def456","category":"Dashboards"}"#
    );
    // The category's audited registry write is a precondition, so it reaches org_categories
    // before the artifact is retagged and cannot be silently skipped.
    assert_eq!(
        category.calls(),
        [
            "resolve_viewer",
            "find_meta",
            "add_category(acme,Dashboards)",
            "set_category"
        ]
    );

    let mut owned = meta("acme");
    owned.owner_email = Some("viewer@example.test".to_owned());
    let visibility = Arc::new(RouteHarness::new(viewer("acme", false), Some(owned)));
    let response = build_router(deps(visibility.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/visibility")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"hidden":true}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"id":"abc123def456","hidden":true}"#
    );
    assert_eq!(
        visibility.calls(),
        ["resolve_viewer", "find_meta", "set_hidden"]
    );
}

#[tokio::test]
async fn category_registration_audit_failure_prevents_browser_category_and_move_success() {
    let category = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    category.fail_category_registration();
    let category_response = build_router(deps(category.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/category")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"category":"Dashboards"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(
        category_response.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        category.calls(),
        [
            "resolve_viewer",
            "find_meta",
            "add_category(acme,Dashboards)"
        ],
        "a failed audited registry write must not retag the artifact"
    );

    let mover = Arc::new(RouteHarness::new(viewer("acme", true), Some(meta("acme"))));
    mover.fail_category_registration();
    let move_response = build_router(deps(mover.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/move")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"org":"beta","category":"Moved"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(move_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        mover.calls(),
        ["resolve_viewer", "find_meta", "add_category(beta,Moved)"],
        "a failed audited registry write must not move the artifact"
    );
}

#[tokio::test]
async fn category_registration_audit_failure_is_an_explicit_mcp_error_before_retagging() {
    let harness = Arc::new(
        RouteHarness::new(viewer("acme", false), Some(meta("acme"))).with_publisher(
            PublisherIdentity {
                client_id: ClientId::from("publisher"),
                org: OrgId::from("acme"),
                label: "Publisher".to_owned(),
                role: "author".to_owned(),
                scopes: Some(["artifacts:publish".to_owned()].into_iter().collect()),
            },
        ),
    );
    harness.fail_category_registration();
    let response = build_router(deps(harness.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"set_category","arguments":{"id":"abc123def456","category":"Dashboards"}}}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = body(response).await;
    assert!(
        String::from_utf8_lossy(&response_body).contains("internal error"),
        "the audit/registry failure must reach the MCP client: {}",
        String::from_utf8_lossy(&response_body)
    );
    assert_eq!(
        harness.calls(),
        ["find_meta", "add_category(acme,Dashboards)"],
        "MCP must not retag when its audited registry precondition fails"
    );
}

#[tokio::test]
async fn share_routes_use_the_share_service_and_node_response_shapes() {
    let create = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(create.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/share")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"expires":"never"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"token":"share-token","expires_at":null,"url":"http://localhost:3480/s/share-token"}"#
    );
    assert_eq!(
        create.calls(),
        ["resolve_viewer", "find_meta", "create_share"]
    );

    let list = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(list.clone()))
        .oneshot(
            Request::builder()
                .uri("/abc123def456/shares")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"shares":[{"token":"share-token","expires_at":null,"created_at":"2026-07-20 00:00:00","created_by":"viewer@example.test"}]}"#
    );
    assert_eq!(list.calls(), ["resolve_viewer", "find_meta", "list_shares"]);

    let revoke = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(revoke.clone()))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/abc123def456/shares/share-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"token":"share-token","revoked":true}"#
    );
    assert_eq!(
        revoke.calls(),
        ["resolve_viewer", "find_meta", "revoke_share"]
    );
}

#[tokio::test]
async fn history_and_restore_routes_preserve_node_numbers_and_notifications() {
    let history = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(history.clone()))
        .oneshot(
            Request::builder()
                .uri("/abc123def456/history")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value =
        serde_json::from_slice(&body(response).await).expect("history JSON");
    assert_eq!(payload["current"], 2);
    assert_eq!(payload["revisions"][0]["revision"], 1);
    assert_eq!(payload["revisions"][0]["is_bundle"], 0);
    assert_eq!(
        history.calls(),
        ["resolve_viewer", "find_meta", "list_revisions"]
    );

    let restore = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(restore.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/restore")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"revision":"1"}"#))
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body(response).await,
        br#"{"id":"abc123def456","revision":3,"restoredFrom":1}"#
    );
    assert_eq!(restore.calls(), ["resolve_viewer", "find_meta", "restore"]);
    assert!(restore.notifications().is_empty());
}

#[tokio::test]
async fn cross_org_feedback_listing_is_concealed_before_the_feedback_service() {
    let harness = Arc::new(RouteHarness::new(viewer("beta", false), Some(meta("acme"))));
    let response = build_router(deps(harness.clone()))
        .oneshot(
            Request::builder()
                .uri("/abc123def456/feedback")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(body(response).await, br#"{"error":"Not found"}"#);
    assert_eq!(harness.calls(), ["resolve_viewer", "find_meta"]);
}

#[tokio::test]
async fn feedback_submission_uses_u11_without_invoking_the_legacy_notifier() {
    let harness = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(harness.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/feedback")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"body":" Note ","anchor":null}"#))
                .expect("request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let payload: serde_json::Value =
        serde_json::from_slice(&body(response).await).expect("feedback JSON");
    assert_eq!(payload["id"], "feedback-id");
    assert_eq!(payload["artifact_id"], "abc123def456");
    assert_eq!(payload["body"], "Note");
    assert_eq!(payload["anchor_approx"], 0);
    assert!(payload.get("org").is_none());
    assert_eq!(
        harness.feedback_submission(),
        Some(SubmitFeedback {
            viewer_email: EmailAddress::from("viewer@example.test"),
            body: " Note ".to_owned(),
            parent_id: None,
            anchor: None,
            anchor_path: None,
            anchor_page: None,
        })
    );
    assert_eq!(
        harness.calls(),
        ["resolve_viewer", "find_meta", "submit_feedback"]
    );
    assert!(harness.notifications().is_empty());
}

#[tokio::test]
async fn feedback_mutations_delegate_ownership_without_legacy_resolve_notification() {
    let delete = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let response = build_router(deps(delete.clone()))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/abc123def456/feedback/feedback-id")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        body(response).await,
        br#"{"id":"feedback-id","deleted":true}"#
    );
    assert_eq!(
        delete.calls(),
        ["resolve_viewer", "find_meta", "delete_feedback"]
    );

    let resolve = Arc::new(RouteHarness::new(viewer("acme", true), Some(meta("acme"))));
    let response = build_router(deps(resolve.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/feedback/feedback-id/resolve")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        body(response).await,
        br#"{"id":"feedback-id","resolved":true}"#
    );
    assert_eq!(
        resolve.calls(),
        ["resolve_viewer", "find_meta", "resolve_feedback_as_viewer"]
    );
    assert!(resolve.notifications().is_empty());
}

#[tokio::test]
async fn mcp_feedback_submit_and_resolve_do_not_invoke_the_legacy_notifier() {
    let harness = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let dependencies = deps(harness.clone());
    let publisher = PublisherIdentity {
        client_id: ClientId::from("publisher"),
        org: OrgId::from("acme"),
        label: "Publisher".to_owned(),
        role: "author".to_owned(),
        scopes: None,
    };
    let submitted: OrderedJson = serde_json::from_str(
        r#"{
          "jsonrpc":"2.0",
          "id":"submit",
          "method":"tools/call",
          "params":{
            "name":"submit_feedback",
            "arguments":{"id":"abc123def456","body":"MCP note"},
            "_meta":{
              "io.modelcontextprotocol/protocolVersion":"2026-07-28",
              "io.modelcontextprotocol/clientInfo":{"name":"u19","version":"1"},
              "io.modelcontextprotocol/clientCapabilities":{
                "extensions":{
                  "io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}
                }
              }
            }
          }
        }"#,
    )
    .expect("submit request");
    let result = dispatch_for_era(&submitted, &publisher, &dependencies, ProtocolEra::Modern)
        .await
        .expect("submit feedback");
    assert_eq!(result["structuredContent"]["submitted"], true);
    assert_eq!(
        harness.calls(),
        ["find_meta", "submit_feedback"],
        "the MCP submit path must stop at the transactional engagement boundary"
    );
    assert!(harness.notifications().is_empty());

    harness.calls.lock().expect("calls lock").clear();
    let resolved: OrderedJson = serde_json::from_str(
        r#"{
          "jsonrpc":"2.0",
          "id":"resolve",
          "method":"tools/call",
          "params":{
            "name":"resolve_feedback",
            "arguments":{"feedback_id":"feedback-id"}
          }
        }"#,
    )
    .expect("resolve request");
    let result = dispatch(&resolved, &publisher, &dependencies)
        .await
        .expect("resolve feedback");
    assert_eq!(result["structuredContent"]["resolved"], true);
    assert_eq!(
        harness.calls(),
        ["feedback_ref", "find_meta", "resolve_feedback_as_publisher"]
    );
    assert!(harness.notifications().is_empty());
}

#[tokio::test]
async fn every_u19_route_conceals_cross_org_unsigned_and_missing_before_subordinate_work() {
    let routes = [
        ("DELETE", "/abc123def456", None),
        ("POST", "/abc123def456/react", Some(r#"{"favorite":true}"#)),
        (
            "POST",
            "/abc123def456/category",
            Some(r#"{"category":"Reports"}"#),
        ),
        (
            "POST",
            "/abc123def456/visibility",
            Some(r#"{"hidden":true}"#),
        ),
        ("POST", "/abc123def456/move", Some(r#"{"org":"beta"}"#)),
        (
            "POST",
            "/abc123def456/share",
            Some(r#"{"expires":"never"}"#),
        ),
        ("GET", "/abc123def456/shares", None),
        ("DELETE", "/abc123def456/shares/token", None),
        ("GET", "/abc123def456/feedback", None),
        ("POST", "/abc123def456/feedback", Some(r#"{"body":"note"}"#)),
        ("DELETE", "/abc123def456/feedback/feedback-id", None),
        ("POST", "/abc123def456/feedback/feedback-id/resolve", None),
        ("GET", "/abc123def456/history", None),
        ("POST", "/abc123def456/restore", Some(r#"{"revision":1}"#)),
    ];

    for (method, uri, request_body) in routes {
        let cases = [
            ("cross-org", viewer("beta", false), Some(meta("acme"))),
            ("unsigned", Viewer::default(), Some(meta("acme"))),
            ("missing", viewer("acme", false), None),
        ];
        for (case, viewer, artifact) in cases {
            let harness = Arc::new(RouteHarness::new(viewer, artifact));
            let mut request = Request::builder().method(method).uri(uri);
            let request_body = match request_body {
                Some(value) => {
                    request = request.header("content-type", "application/json");
                    Body::from(value)
                }
                None => Body::empty(),
            };
            let response = build_router(deps(harness.clone()))
                .oneshot(request.body(request_body).expect("request"))
                .await
                .expect("router response");

            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{case} {method} {uri}"
            );
            assert_eq!(
                body(response).await,
                br#"{"error":"Not found"}"#,
                "{case} {method} {uri}"
            );
            assert_eq!(
                harness.calls(),
                ["resolve_viewer", "find_meta"],
                "{case} {method} {uri}"
            );
        }
    }
}

fn node_reference_available(root: &FsPath) -> bool {
    let required = std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1");
    let unavailable = if !root.join("lib/app.js").is_file() {
        Some("lib/app.js is missing")
    } else if !root.join("node_modules/express").is_dir() {
        Some("the Node reference dependencies are missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };
    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !required,
                "{REQUIRE_NODE_REFERENCE}=1 but the Node route reference is unavailable \
                 ({reason}); the U19 HTTP parity proof did not run"
            );
            eprintln!("skipping U19 Node route parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

fn run_node_route_reference(root: &FsPath) -> serde_json::Value {
    let module = format!("file://{}", root.join("lib/app.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_ROUTE_DRIVER)
        .arg(module)
        .output()
        .expect("run Node app route reference");
    assert!(
        output.status.success(),
        "Node app route reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Node app route reference emitted invalid JSON ({error}):\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

async fn rust_route_result(
    method: &str,
    uri: &str,
    request_body: Option<&str>,
    route_viewer: Viewer,
) -> serde_json::Value {
    let harness = Arc::new(RouteHarness::new(route_viewer, Some(meta("acme"))));
    let mut request = Request::builder().method(method).uri(uri);
    let request_body = match request_body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_owned())
        }
        None => Body::empty(),
    };
    let response = build_router(deps(harness))
        .oneshot(request.body(request_body).expect("request"))
        .await
        .expect("router response");
    let status = response.status().as_u16();
    let bytes = body(response).await;
    let payload: serde_json::Value =
        serde_json::from_slice(&bytes).expect("Rust route emitted JSON");
    serde_json::json!({ "status": status, "body": payload })
}

#[tokio::test]
async fn rust_action_responses_match_the_real_node_app_routes() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if !node_reference_available(&root) {
        return;
    }

    let signed = viewer("acme", false);
    let cases = [
        ("delete", "DELETE", "/abc123def456", None, signed.clone()),
        (
            "react",
            "POST",
            "/abc123def456/react",
            Some(r#"{"favorite":0,"vote":-1}"#),
            signed.clone(),
        ),
        (
            "category",
            "POST",
            "/abc123def456/category",
            Some(r#"{"category":"Dashboards"}"#),
            signed.clone(),
        ),
        (
            "visibility",
            "POST",
            "/abc123def456/visibility",
            Some(r#"{"hidden":true}"#),
            signed.clone(),
        ),
        (
            "moveCrossOrg",
            "POST",
            "/abc123def456/move",
            Some(r#"{"org":"beta"}"#),
            viewer("beta", false),
        ),
        (
            "moveSameOrg",
            "POST",
            "/abc123def456/move",
            Some(r#"{"org":"beta"}"#),
            signed.clone(),
        ),
        (
            "shareCreate",
            "POST",
            "/abc123def456/share",
            Some(r#"{"expires":"never"}"#),
            signed.clone(),
        ),
        (
            "shareList",
            "GET",
            "/abc123def456/shares",
            None,
            signed.clone(),
        ),
        (
            "shareRevoke",
            "DELETE",
            "/abc123def456/shares/share-token",
            None,
            signed.clone(),
        ),
        (
            "feedbackList",
            "GET",
            "/abc123def456/feedback",
            None,
            signed.clone(),
        ),
        (
            "feedbackCreate",
            "POST",
            "/abc123def456/feedback",
            Some(r#"{"body":" Note ","anchor":null}"#),
            signed.clone(),
        ),
        (
            "feedbackDelete",
            "DELETE",
            "/abc123def456/feedback/feedback-id",
            None,
            signed.clone(),
        ),
        (
            "feedbackResolve",
            "POST",
            "/abc123def456/feedback/feedback-id/resolve",
            None,
            signed.clone(),
        ),
        (
            "history",
            "GET",
            "/abc123def456/history",
            None,
            signed.clone(),
        ),
        (
            "restore",
            "POST",
            "/abc123def456/restore",
            Some(r#"{"revision":"1"}"#),
            signed,
        ),
    ];
    let mut rust = serde_json::Map::new();
    for (name, method, uri, request_body, route_viewer) in cases {
        rust.insert(
            name.to_owned(),
            rust_route_result(method, uri, request_body, route_viewer).await,
        );
    }

    assert_eq!(
        serde_json::Value::Object(rust),
        run_node_route_reference(&root)
    );
}

#[tokio::test]
async fn bodyless_category_and_move_requests_cannot_clear_an_artifact_category() {
    let category = Arc::new(RouteHarness::new(viewer("acme", false), Some(meta("acme"))));
    let category_response = build_router(deps(category.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/category")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(category_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body(category_response).await,
        br#"{"error":"category is required"}"#
    );
    assert_eq!(category.calls(), ["resolve_viewer", "find_meta"]);

    let move_artifact = Arc::new(RouteHarness::new(viewer("acme", true), Some(meta("acme"))));
    let move_response = build_router(deps(move_artifact.clone()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/abc123def456/move")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(move_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body(move_response).await,
        br#"{"error":"org or category is required"}"#
    );
    assert_eq!(move_artifact.calls(), ["resolve_viewer", "find_meta"]);
}
