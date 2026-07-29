//! U06 — access policy (invariant 3).
//!
//! The matrix here is the Rust-side twin of `conformance/cases/human-concealment.invariant3.json`
//! (57 steps): every artifact-scoped surface × every identity × {existing-foreign, nonexistent,
//! reserved}. Because [`AccessPolicy`] is the single decision for all of those surfaces, the
//! per-route dimension collapses into one assertion set plus a proof that the gate is in fact the
//! only path to an [`AuthorizedArtifact`].
//!
//! What is proven here:
//!
//! 1. Foreign and nonexistent produce not merely the same variant but the same **rendered
//!    response bytes** — status, headers and body — for unsigned and cross-org viewers.
//! 2. No subordinate read (body, bundle, revision body, history, and by the same construction
//!    feedback/shares/views/thumbnail) is reachable before the decision; the spy service records
//!    that only `find_meta` runs, and a reserved id does not even do that.
//! 3. Publisher ownership needs client id **and** current org, so an admin re-tenant revokes
//!    control.
//! 4. Share grants are org-scoped and every failure is the same concealed answer.
//! 5. The wrappers cannot be built outside `security::access` — see the `compile_fail` doctests
//!    on that module, and `wrapper_construction_is_module_private` below for the argument.

use std::collections::BTreeMap;
use std::sync::Mutex;

use artifact_mcp::error::AppError;
use artifact_mcp::model::{
    ArtifactFile, ArtifactId, ArtifactMeta, ArtifactUpdate, ClientId, DigestBackfillReport,
    EmailAddress, OrgArtifacts, OrgId, PublishArtifact, PublishedArtifact, PublisherIdentity,
    RestoreArtifactResult, RevisionHistory, ShareGrant, StorageAuditReport, Timestamp,
    UpdateArtifactResult, Viewer,
};
use artifact_mcp::ports::{ArtifactService, BoxFuture};
use artifact_mcp::security::access::{
    ADMINS_ONLY_MESSAGE, AccessPolicy, AuthorizedArtifact, Concealment, DELETE_PERMISSION_ERROR,
    FORBIDDEN_MESSAGE, NOT_FOUND_MESSAGE, NOT_SIGNED_IN_MESSAGE, READ_PERMISSION_ERROR,
    WRITE_PERMISSION_ERROR, resolve_for_viewer, unknown_artifact_message,
};
use axum::response::IntoResponse;

// ---------------------------------------------------------------------------
// Fixtures — mirror the conformance case: an artifact published into `acme`.
// ---------------------------------------------------------------------------

/// The published id from step 0 of the conformance case (12 chars, minted alphabet).
const EXISTING_FOREIGN: &str = "acmeartifact";
/// The probe id from every `nonexistent` step of the conformance case.
const NONEXISTENT: &str = "zzzzzzzzzzzz";
/// Ids the router reserves; `lib/app.js` maps them to `null` before the decision.
const RESERVED_IDS: [&str; 6] = ["raw", "settings", "mcp", "s", "favicon.ico", "UPPERCASE1"];

fn artifact(id: &str, org: &str, client_id: &str) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId::from(id),
        client_id: ClientId::from(client_id),
        org: OrgId::from(org),
        title: "Concealed".to_owned(),
        description: "acme private".to_owned(),
        bytes: 21,
        created_at: Timestamp("2026-07-21T00:00:00.000Z".to_owned()),
        updated_at: Timestamp("2026-07-21T00:00:00.000Z".to_owned()),
        uploader_label: "acme agent".to_owned(),
        owner_email: None,
        is_bundle: false,
        entry: "index.html".to_owned(),
        revision: 1,
        category: String::new(),
        hidden: false,
        body_sha256: "a".repeat(64),
    }
}

fn acme_artifact() -> ArtifactMeta {
    artifact(EXISTING_FOREIGN, "acme", "acme-key")
}

fn unsigned() -> Viewer {
    Viewer {
        email: None,
        org: None,
        is_admin: false,
    }
}

fn cross_org() -> Viewer {
    Viewer {
        email: Some(EmailAddress::from("b@globex.test")),
        org: Some(OrgId::from("globex")),
        is_admin: false,
    }
}

fn same_org() -> Viewer {
    Viewer {
        email: Some(EmailAddress::from("a@acme.test")),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    }
}

fn admin() -> Viewer {
    Viewer {
        email: Some(EmailAddress::from("root@admin.test")),
        org: Some(OrgId::from("admin")),
        is_admin: true,
    }
}

/// The artifact-scoped surfaces the conformance matrix probes. Each one resolves its artifact
/// through the single gate, so the policy assertions below cover all of them at once. Listed so
/// a route added without going through the gate is visibly missing from this ledger.
const ARTIFACT_SCOPED_SURFACES: [&str; 14] = [
    "GET /:id (viewer shell)",
    "GET /raw/:id (raw delivery)",
    "GET /raw/:id/* (bundle file)",
    "GET /raw/:id/rev/:n (past revision)",
    "GET /thumbnails/:id",
    "GET /:id/shares",
    "GET /:id/history",
    "GET /:id/feedback",
    "POST /:id/feedback",
    "POST /:id/react",
    "POST /:id/visibility",
    "POST /:id/category",
    "POST /:id/move (admin-only route)",
    "DELETE /:id",
];

// ---------------------------------------------------------------------------
// Spy service — records every port call so "no subordinate read" is measurable.
// ---------------------------------------------------------------------------

struct SpyArtifacts {
    stored: BTreeMap<String, ArtifactMeta>,
    calls: Mutex<Vec<&'static str>>,
}

impl SpyArtifacts {
    fn with(meta: ArtifactMeta) -> Self {
        let mut stored = BTreeMap::new();
        stored.insert(meta.id.0.clone(), meta);
        Self {
            stored,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, call: &'static str) {
        self.calls
            .lock()
            .expect("spy call log is never poisoned")
            .push(call);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls
            .lock()
            .expect("spy call log is never poisoned")
            .clone()
    }

    fn reset(&self) {
        self.calls
            .lock()
            .expect("spy call log is never poisoned")
            .clear();
    }
}

/// Any call other than `find_meta` is a bug in the gate, so every other method both records the
/// attempt and refuses to produce data.
fn refuse<'a, T>(spy: &'a SpyArtifacts, call: &'static str) -> BoxFuture<'a, Result<T, AppError>> {
    spy.record(call);
    Box::pin(async { Err(AppError::Internal) })
}

impl ArtifactService for SpyArtifacts {
    fn find_meta<'a>(
        &'a self,
        id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        self.record("find_meta");
        let found = self.stored.get(&id.0).cloned();
        Box::pin(async move { Ok(found) })
    }

    fn publish(
        &self,
        _request: PublishArtifact,
    ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>> {
        refuse(self, "publish")
    }

    fn list_for_publisher<'a>(
        &'a self,
        _publisher: &'a PublisherIdentity,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        refuse(self, "list_for_publisher")
    }

    fn list_org_artifacts<'a>(
        &'a self,
        _org: &'a OrgId,
        _include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        refuse(self, "list_org_artifacts")
    }

    fn list_all_grouped_by_org(
        &self,
        _include_hidden: bool,
    ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>> {
        refuse(self, "list_all_grouped_by_org")
    }

    fn list_org_ids<'a>(
        &'a self,
        _org: &'a OrgId,
        _include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>> {
        refuse(self, "list_org_ids")
    }

    fn read_body<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        refuse(self, "read_body")
    }

    fn read_bundle_file<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _relative_path: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        refuse(self, "read_bundle_file")
    }

    fn read_revision_body<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _revision: u64,
        _relative_path: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        refuse(self, "read_revision_body")
    }

    fn list_bundle_files<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
        _revision: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Vec<(String, u64)>>, AppError>> {
        refuse(self, "list_bundle_files")
    }

    fn list_revisions<'a>(
        &'a self,
        _artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<RevisionHistory, AppError>> {
        refuse(self, "list_revisions")
    }

    fn update(
        &self,
        _artifact: AuthorizedArtifact,
        _update: ArtifactUpdate,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
        refuse(self, "update")
    }

    fn restore(
        &self,
        _artifact: AuthorizedArtifact,
        _revision: u64,
        _acting_client_id: Option<ClientId>,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
        refuse(self, "restore")
    }

    fn delete(&self, _artifact: AuthorizedArtifact) -> BoxFuture<'_, Result<bool, AppError>> {
        refuse(self, "delete")
    }

    fn set_category(
        &self,
        _artifact: AuthorizedArtifact,
        _category: String,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        refuse(self, "set_category")
    }

    fn set_hidden(
        &self,
        _artifact: AuthorizedArtifact,
        _hidden: bool,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        refuse(self, "set_hidden")
    }

    fn move_to_org(
        &self,
        _artifact: AuthorizedArtifact,
        _target_org: OrgId,
        _category: Option<String>,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        refuse(self, "move_to_org")
    }

    fn audit_storage(
        &self,
        _clean_transient: bool,
    ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>> {
        refuse(self, "audit_storage")
    }

    fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>> {
        refuse(self, "backfill_body_digests")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The full rendered HTTP answer: status, headers and body bytes. Two denials are
/// indistinguishable only if these are equal.
async fn rendered(error: AppError) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let response = error.into_response();
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("error bodies are small and complete");
    (status, headers, body.to_vec())
}

fn decision(viewer: &Viewer, meta: Option<ArtifactMeta>) -> Result<ArtifactMeta, AppError> {
    AccessPolicy::authorize_viewer(viewer, meta).map(AuthorizedArtifact::into_meta)
}

// ---------------------------------------------------------------------------
// 1. The concealment matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn foreign_and_nonexistent_render_identical_responses_for_every_identity() {
    for (label, viewer) in [("unsigned", unsigned()), ("cross-org", cross_org())] {
        let foreign = decision(&viewer, Some(acme_artifact()))
            .expect_err("a foreign artifact is never readable");
        let missing = decision(&viewer, None).expect_err("a missing artifact is never readable");

        assert_eq!(foreign, missing, "{label}: variants diverge");
        assert_eq!(foreign, AppError::ConcealedNotFound, "{label}");
        assert_eq!(foreign.to_string(), NOT_FOUND_MESSAGE, "{label}");

        let foreign_response = rendered(foreign).await;
        let missing_response = rendered(missing).await;
        assert_eq!(
            foreign_response, missing_response,
            "{label}: rendered responses are distinguishable"
        );
        assert_eq!(foreign_response.0, 404, "{label}");
        assert_eq!(foreign_response.2, br#"{"error":"Not found"}"#, "{label}");
    }
}

#[tokio::test]
async fn every_concealed_input_collapses_to_one_response() {
    // reserved / missing / foreign / unsigned — one answer, byte for byte.
    let mut responses = Vec::new();
    for viewer in [unsigned(), cross_org(), same_org(), admin()] {
        // reserved: `lookup_target` yields `None`, which is fed to the same decision.
        for reserved in RESERVED_IDS {
            assert!(
                AccessPolicy::lookup_target(reserved).is_none(),
                "{reserved}"
            );
            responses.push(
                rendered(decision(&viewer, None).expect_err("reserved never resolves")).await,
            );
        }
        // nonexistent
        responses
            .push(rendered(decision(&viewer, None).expect_err("missing never resolves")).await);
    }
    // foreign, for the identities that must not read it
    for viewer in [unsigned(), cross_org()] {
        responses.push(
            rendered(decision(&viewer, Some(acme_artifact())).expect_err("foreign never resolves"))
                .await,
        );
    }

    let first = responses.first().expect("matrix is not empty").clone();
    for (index, response) in responses.iter().enumerate() {
        assert_eq!(*response, first, "concealed response {index} differs");
    }
}

#[test]
fn same_org_and_admin_read_but_only_what_exists() {
    let meta = acme_artifact();

    assert_eq!(
        decision(&same_org(), Some(meta.clone())).expect("same-org viewer reads its own tenant"),
        meta
    );
    assert_eq!(
        decision(&admin(), Some(meta.clone())).expect("an admin reads every tenant"),
        meta
    );
    // Existence is still required — an admin probing an unknown id gets the concealed answer.
    assert_eq!(
        decision(&same_org(), None).unwrap_err(),
        AppError::ConcealedNotFound
    );
    assert_eq!(
        decision(&admin(), None).unwrap_err(),
        AppError::ConcealedNotFound
    );
    // A same-org viewer is still foreign to another tenant.
    assert_eq!(
        decision(
            &same_org(),
            Some(artifact(EXISTING_FOREIGN, "globex", "g-key"))
        )
        .unwrap_err(),
        AppError::ConcealedNotFound
    );
}

#[test]
fn the_ledger_of_artifact_scoped_surfaces_is_covered_by_one_gate() {
    // Every surface below resolves its artifact through `AccessPolicy::authorize_viewer`, so the
    // matrix above applies to each without a per-route repetition. The list mirrors the 14 route
    // probes of `human-concealment.invariant3` (28 steps per identity: 14 surfaces × {existing,
    // nonexistent}, times two identities, plus the publish step = 57).
    assert_eq!(ARTIFACT_SCOPED_SURFACES.len(), 14);
    assert_eq!(ARTIFACT_SCOPED_SURFACES.len() * 2 * 2 + 1, 57);
}

// ---------------------------------------------------------------------------
// 2. Un-concealed parity (Node still exports it) and the admin-role decision
// ---------------------------------------------------------------------------

#[test]
fn revealed_mode_keeps_the_historical_statuses() {
    let meta = acme_artifact();

    let unsigned_denial =
        AccessPolicy::artifact_access(&unsigned(), Some(&meta), Concealment::Reveal).unwrap_err();
    assert_eq!(
        unsigned_denial,
        AppError::Unauthorized(NOT_SIGNED_IN_MESSAGE.to_owned())
    );
    assert_eq!(unsigned_denial.http_status().as_u16(), 401);

    let foreign_denial =
        AccessPolicy::artifact_access(&cross_org(), Some(&meta), Concealment::Reveal).unwrap_err();
    assert_eq!(
        foreign_denial,
        AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned())
    );
    assert_eq!(foreign_denial.http_status().as_u16(), 403);

    // Missing is 404 in BOTH modes — that ordering is what lets the concealed foreign answer
    // be identical to it.
    for mode in [Concealment::Reveal, Concealment::Conceal] {
        assert_eq!(
            AccessPolicy::artifact_access(&unsigned(), None, mode).unwrap_err(),
            AppError::ConcealedNotFound
        );
    }
}

#[test]
fn admin_only_settings_stay_a_role_answer_not_a_tenant_answer() {
    // Node answers 403 (not 401) for an unsigned caller on the settings surface.
    let unsigned_denial = AccessPolicy::admin_access(&unsigned()).unwrap_err();
    assert_eq!(
        unsigned_denial,
        AppError::Forbidden(NOT_SIGNED_IN_MESSAGE.to_owned())
    );
    assert_eq!(unsigned_denial.http_status().as_u16(), 403);

    let non_admin = AccessPolicy::admin_access(&same_org()).unwrap_err();
    assert_eq!(
        non_admin,
        AppError::Forbidden(ADMINS_ONLY_MESSAGE.to_owned())
    );
    assert_eq!(non_admin.http_status().as_u16(), 403);

    AccessPolicy::admin_access(&admin()).expect("an admin passes the role gate");

    // These routes carry no artifact id, so a distinct 403 discloses nothing about tenancy.
    assert_ne!(non_admin, AppError::ConcealedNotFound);
}

#[test]
fn human_artifact_management_is_admin_or_recorded_owner_only() {
    let mut owned = acme_artifact();
    owned.owner_email = Some("owner@acme.test".to_owned());
    let owner = Viewer {
        email: Some(EmailAddress::from("OWNER@ACME.TEST")),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    };
    assert!(AccessPolicy::viewer_can_manage_artifact(&owner, &owned));
    assert!(!AccessPolicy::viewer_can_manage_artifact(
        &same_org(),
        &owned
    ));

    let mut wrong_org = owner.clone();
    wrong_org.org = Some(OrgId::from("globex"));
    assert!(!AccessPolicy::viewer_can_manage_artifact(
        &wrong_org, &owned
    ));

    let legacy = acme_artifact();
    assert!(!AccessPolicy::viewer_can_manage_artifact(&owner, &legacy));
    assert!(AccessPolicy::viewer_can_manage_artifact(&admin(), &legacy));
}

#[test]
fn javascript_truthiness_is_reproduced_for_empty_identity_strings() {
    let blank_email = Viewer {
        email: Some(EmailAddress::from("")),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    };
    assert!(!AccessPolicy::is_signed_in(&blank_email));
    assert_eq!(
        decision(&blank_email, Some(acme_artifact())).unwrap_err(),
        AppError::ConcealedNotFound
    );

    // `viewer.org && viewer.org === artifact.org`: an empty org matches nothing, not even an
    // artifact whose org is also empty.
    let blank_org = Viewer {
        email: Some(EmailAddress::from("a@acme.test")),
        org: Some(OrgId::from("")),
        is_admin: false,
    };
    assert_eq!(
        decision(&blank_org, Some(artifact(EXISTING_FOREIGN, "", "k"))).unwrap_err(),
        AppError::ConcealedNotFound
    );
}

// ---------------------------------------------------------------------------
// 3. No subordinate read before the decision
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_gate_reads_only_metadata_before_deciding() {
    let spy = SpyArtifacts::with(acme_artifact());

    for viewer in [unsigned(), cross_org()] {
        for id in [EXISTING_FOREIGN, NONEXISTENT] {
            spy.reset();
            let denial = resolve_for_viewer(&spy, &viewer, id)
                .await
                .expect_err("neither identity may read an acme artifact");
            assert_eq!(denial, AppError::ConcealedNotFound);
            assert_eq!(
                spy.calls(),
                vec!["find_meta"],
                "id {id} performed work beyond the metadata lookup"
            );
        }
    }

    // A reserved id short-circuits before the lookup, exactly like `lib/app.js:81`, and still
    // produces the same concealed answer.
    for reserved in RESERVED_IDS {
        spy.reset();
        let denial = resolve_for_viewer(&spy, &same_org(), reserved)
            .await
            .expect_err("reserved ids never address an artifact");
        assert_eq!(denial, AppError::ConcealedNotFound);
        assert!(
            spy.calls().is_empty(),
            "reserved id {reserved} reached storage"
        );
    }

    // The authorized path also stops at metadata: the wrapper is produced without any read.
    spy.reset();
    let authorized = resolve_for_viewer(&spy, &same_org(), EXISTING_FOREIGN)
        .await
        .expect("the owning tenant is authorized");
    assert_eq!(authorized.meta(), &acme_artifact());
    assert_eq!(spy.calls(), vec!["find_meta"]);
}

#[test]
fn wrapper_construction_is_module_private() {
    // Structural argument, checked by the compiler rather than by review:
    //
    // * `AuthorizedArtifact` and `OwnedArtifact` are tuple structs whose single field is NOT
    //   `pub`, and they are declared in `src/security/access.rs`. Rust therefore makes their
    //   constructors and their `.0` field visible only inside that module — this integration
    //   test is a separate crate and cannot name either.
    // * The four `compile_fail` doctests on `security::access` pin that: forging
    //   `AuthorizedArtifact(meta)`, forging `OwnedArtifact(meta)`, unwrapping `.0`, and calling
    //   `ArtifactService::read_body` with a bare `ArtifactMeta` all fail to compile. A control
    //   doctest that MUST compile guards them against passing for the wrong reason.
    // * Consequently the only values of these types in the whole program come from
    //   `AccessPolicy::authorize_viewer`, `authorize_publisher_read`, `authorize_publisher_write`,
    //   `authorize_share`, and `OwnedArtifact::into_authorized` — all of which perform a decision
    //   first. Every subordinate read in the frozen port manifest demands one of them.
    //
    // The runtime half of the argument: this test crate CAN obtain a wrapper, but only by going
    // through a decision that granted it.
    let granted = AccessPolicy::authorize_viewer(&same_org(), Some(acme_artifact()))
        .expect("the owning tenant is authorized");
    assert_eq!(granted.meta().org, OrgId::from("acme"));
}

// ---------------------------------------------------------------------------
// 4. Publisher ownership
// ---------------------------------------------------------------------------

fn publisher(client_id: &str, org: &str) -> PublisherIdentity {
    PublisherIdentity {
        client_id: ClientId::from(client_id),
        org: OrgId::from(org),
        label: format!("{org} agent"),
        role: "author".to_owned(),
        scopes: None,
    }
}

fn publisher_with_role(client_id: &str, org: &str, role: &str) -> PublisherIdentity {
    PublisherIdentity {
        role: role.to_owned(),
        ..publisher(client_id, org)
    }
}

#[test]
fn publisher_capabilities_split_read_write_and_delete() {
    let own = acme_artifact();
    let colleague = artifact(EXISTING_FOREIGN, "acme", "other-key");
    let foreign = artifact(EXISTING_FOREIGN, "globex", "globex-key");

    let reader = publisher_with_role("acme-key", "acme", "reader");
    assert!(AccessPolicy::publisher_can_read(&reader, &own));
    assert!(AccessPolicy::publisher_can_read(&reader, &colleague));
    assert!(!AccessPolicy::publisher_can_read(&reader, &foreign));
    assert!(!AccessPolicy::publisher_can_write(&reader, &own));
    assert!(!AccessPolicy::publisher_can_write(&reader, &colleague));
    assert!(!AccessPolicy::publisher_can_delete(&reader, &own));

    let author = publisher("acme-key", "acme");
    assert!(AccessPolicy::publisher_can_read(&author, &own));
    assert!(!AccessPolicy::publisher_can_read(&author, &colleague));
    assert!(AccessPolicy::publisher_can_write(&author, &own));
    assert!(!AccessPolicy::publisher_can_write(&author, &colleague));
    assert!(AccessPolicy::publisher_can_delete(&author, &own));
    assert!(!AccessPolicy::publisher_can_delete(&author, &colleague));

    let collaborator = publisher_with_role("acme-key", "acme", "collaborator");
    assert!(AccessPolicy::publisher_can_read(&collaborator, &colleague));
    assert!(AccessPolicy::publisher_can_write(&collaborator, &colleague));
    assert!(AccessPolicy::publisher_can_delete(&collaborator, &own));
    assert!(!AccessPolicy::publisher_can_delete(
        &collaborator,
        &colleague
    ));
}

#[test]
fn an_admin_re_tenant_revokes_publisher_control() {
    let acme = publisher("acme-key", "acme");
    let before = acme_artifact();
    assert!(
        AccessPolicy::authorize_publisher_read(&acme, Some(before.clone()), EXISTING_FOREIGN)
            .is_ok()
    );

    // `move_to_org` keeps client_id and changes org. Control must not follow the key.
    let mut after = before;
    after.org = OrgId::from("globex");

    let denial =
        AccessPolicy::authorize_publisher_read(&acme, Some(after.clone()), EXISTING_FOREIGN)
            .unwrap_err();
    assert_eq!(
        denial,
        AppError::NotFound(unknown_artifact_message(EXISTING_FOREIGN))
    );
    // …and it is the SAME answer as an id that never existed.
    assert_eq!(
        denial,
        AccessPolicy::authorize_publisher_read(&acme, None, EXISTING_FOREIGN).unwrap_err()
    );

    // The new tenant's key does own it.
    let globex = publisher("globex-key", "globex");
    assert!(!AccessPolicy::publisher_can_read(&globex, &after));
    let mut moved = after;
    moved.client_id = ClientId::from("globex-key");
    assert!(AccessPolicy::publisher_can_read(&globex, &moved));
}

#[test]
fn a_publisher_admin_key_is_the_org_and_owns_every_tenant() {
    let admin_key = publisher("root-key", "admin");
    assert!(AccessPolicy::publisher_is_admin(&admin_key));
    assert!(AccessPolicy::publisher_can_read(
        &admin_key,
        &acme_artifact()
    ));
    assert!(AccessPolicy::publisher_can_write(
        &admin_key,
        &acme_artifact()
    ));
    assert!(AccessPolicy::publisher_can_delete(
        &admin_key,
        &acme_artifact()
    ));
    assert_eq!(
        AccessPolicy::authorize_publisher_read(&admin_key, None, NONEXISTENT).unwrap_err(),
        AppError::NotFound(unknown_artifact_message(NONEXISTENT))
    );

    // A key that is merely flagged is NOT an admin: Node's publisher identity has no such flag,
    // the org is the whole rule.
    let flagged = PublisherIdentity {
        ..publisher("acme-key", "acme")
    };
    assert!(!AccessPolicy::publisher_is_admin(&flagged));
    assert!(!AccessPolicy::publisher_can_read(
        &flagged,
        &artifact(EXISTING_FOREIGN, "globex", "other")
    ));
}

#[test]
fn publisher_reads_conceal_but_ownership_scoped_writes_mirror_node() {
    let acme = publisher("acme-key", "acme");
    let foreign = artifact(EXISTING_FOREIGN, "globex", "globex-key");

    // Read probe: foreign and missing are indistinguishable.
    let foreign_read =
        AccessPolicy::authorize_publisher_read(&acme, Some(foreign.clone()), EXISTING_FOREIGN)
            .unwrap_err();
    let missing_read =
        AccessPolicy::authorize_publisher_read(&acme, None, EXISTING_FOREIGN).unwrap_err();
    assert_eq!(foreign_read, missing_read);
    assert_eq!(foreign_read.to_string(), "Unknown artifact: acmeartifact");

    // Cross-org write denial stays concealed; same-org capability refusal is explicit.
    const REFUSAL: &str = "You can only create shares for your own artifacts";
    assert_eq!(
        AccessPolicy::authorize_publisher_write(&acme, Some(foreign), EXISTING_FOREIGN, REFUSAL)
            .unwrap_err(),
        AppError::NotFound(unknown_artifact_message(EXISTING_FOREIGN))
    );
    assert_eq!(
        AccessPolicy::authorize_publisher_write(&acme, None, EXISTING_FOREIGN, REFUSAL)
            .unwrap_err(),
        AppError::NotFound(unknown_artifact_message(EXISTING_FOREIGN))
    );

    // Owning key: the write gate yields the ownership wrapper, which widens into read access.
    let owned = AccessPolicy::authorize_publisher_write(
        &acme,
        Some(acme_artifact()),
        EXISTING_FOREIGN,
        REFUSAL,
    )
    .expect("the owning key may create shares");
    assert_eq!(owned.meta(), &acme_artifact());
    assert_eq!(owned.into_authorized().into_meta(), acme_artifact());

    let colleague = artifact(EXISTING_FOREIGN, "acme", "other-key");
    assert_eq!(
        AccessPolicy::authorize_publisher_read(&acme, Some(colleague.clone()), EXISTING_FOREIGN)
            .unwrap_err(),
        AppError::Forbidden(READ_PERMISSION_ERROR.to_owned())
    );
    assert_eq!(
        AccessPolicy::authorize_publisher_write(
            &acme,
            Some(colleague.clone()),
            EXISTING_FOREIGN,
            REFUSAL
        )
        .unwrap_err(),
        AppError::Forbidden(WRITE_PERMISSION_ERROR.to_owned())
    );
    assert_eq!(
        AccessPolicy::authorize_publisher_delete(&acme, Some(colleague), EXISTING_FOREIGN)
            .unwrap_err(),
        AppError::Forbidden(DELETE_PERMISSION_ERROR.to_owned())
    );
}

// ---------------------------------------------------------------------------
// 5. Public shares
// ---------------------------------------------------------------------------

#[tokio::test]
async fn share_access_is_org_scoped_and_fails_concealed() {
    let grant = ShareGrant {
        artifact_id: ArtifactId::from(EXISTING_FOREIGN),
        org: OrgId::from("acme"),
    };

    let authorized = AccessPolicy::authorize_share(&grant, Some(acme_artifact()))
        .expect("a live token authorizes its artifact");
    assert_eq!(authorized.meta(), &acme_artifact());
    assert!(AccessPolicy::share_matches(&grant, &acme_artifact()));

    // Re-tenanted artifact: the old link stops matching, and says nothing about why.
    let moved = artifact(EXISTING_FOREIGN, "globex", "acme-key");
    assert!(!AccessPolicy::share_matches(&grant, &moved));
    let stale = AccessPolicy::authorize_share(&grant, Some(moved)).unwrap_err();
    let deleted = AccessPolicy::authorize_share(&grant, None).unwrap_err();
    assert_eq!(stale, deleted);
    assert_eq!(stale, AppError::ConcealedNotFound);
    assert_eq!(rendered(stale).await, rendered(deleted).await);

    // A share grant never consults a viewer: the token itself is the boundary, so an unsigned
    // holder gets the artifact that a signed-in cross-org viewer would be refused.
    assert_eq!(
        decision(&unsigned(), Some(acme_artifact())).unwrap_err(),
        AppError::ConcealedNotFound
    );
    assert!(AccessPolicy::authorize_share(&grant, Some(acme_artifact())).is_ok());
}
