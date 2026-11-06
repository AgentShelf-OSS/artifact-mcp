//! Access policy and concealed-read decisions.
//!
//! # Invariant 3 lives here, once
//!
//! A cross-organization read must be indistinguishable from a read of an id that does not
//! exist. Returning `403` for a foreign-but-existing artifact would confirm that the id is
//! real in *some* tenant, which is exactly the disclosure invariant 3 forbids. So for an
//! unsigned or cross-org human, these four inputs collapse to **one** result — the concealed
//! `404` — on every artifact-scoped route:
//!
//! | input | collapses to |
//! |---|---|
//! | reserved id (`raw`, `settings`, bad shape, …) | [`AppError::ConcealedNotFound`] |
//! | id that does not exist | [`AppError::ConcealedNotFound`] |
//! | id that exists in another organization | [`AppError::ConcealedNotFound`] |
//! | any id at all, viewer not signed in | [`AppError::ConcealedNotFound`] |
//!
//! Role-only *settings* decisions ([`AccessPolicy::admin_access`]) keep their distinct `403`
//! because that answer is about the caller's role, not about which tenant owns a record — and
//! those routes carry no artifact id, so there is nothing to conceal.
//!
//! # Structural enforcement (A7)
//!
//! [`AuthorizedArtifact`] and [`OwnedArtifact`] have private fields and this module holds the
//! **only** constructors ([`AccessPolicy::authorize_viewer`],
//! [`AccessPolicy::authorize_publisher_read`], [`AccessPolicy::authorize_publisher_write`],
//! [`AccessPolicy::authorize_share`], plus the frozen `OwnedArtifact::into_authorized`). Every
//! subordinate read in the frozen port manifest — body, bundle file, revision body, history,
//! reaction, view, feedback, share, thumbnail — takes one of those wrappers, so a route that
//! reads before deciding does not compile. Only `ArtifactService::find_meta` and
//! `EngagementService::feedback_ref` take a bare id, and both return minimal metadata by
//! contract.
//!
//! Bypass attempts are rejected by the compiler, not by review. The four `compile_fail` blocks
//! below are guarded by this **control** block, which must compile: it names every path the
//! negative blocks use and performs the sanctioned versions of the same operations. Without it
//! a `compile_fail` block would also "pass" on a typo or a stale import, and the
//! `compile_fail,E0xxx` error-code annotation is advisory in the current rustdoc (verified: a
//! deliberately wrong code still passes), so the control is what pins the reason.
//!
//! ```
//! use artifact_mcp::error::AppError;
//! use artifact_mcp::model::{ArtifactMeta, Viewer};
//! use artifact_mcp::ports::ArtifactService;
//! use artifact_mcp::security::access::{AccessPolicy, AuthorizedArtifact, OwnedArtifact};
//!
//! // The only sanctioned way to obtain the viewer-side wrapper.
//! fn authorize(viewer: &Viewer, meta: Option<ArtifactMeta>)
//!     -> Result<AuthorizedArtifact, AppError> {
//!     AccessPolicy::authorize_viewer(viewer, meta)
//! }
//!
//! // Both wrapper types exist, and ownership widens into read authorization.
//! fn widen(owned: OwnedArtifact) -> AuthorizedArtifact {
//!     owned.into_authorized()
//! }
//!
//! // A subordinate read is reachable — but only through the wrapper.
//! async fn read_after_deciding(service: &dyn ArtifactService, authorized: &AuthorizedArtifact) {
//!     let _body = service.read_body(authorized).await;
//! }
//! ```
//!
//! ```compile_fail,E0603
//! // The tuple-struct constructor is private to `security::access`.
//! use artifact_mcp::model::ArtifactMeta;
//! use artifact_mcp::security::access::AuthorizedArtifact;
//! fn forge(meta: ArtifactMeta) -> AuthorizedArtifact {
//!     AuthorizedArtifact(meta)
//! }
//! ```
//!
//! ```compile_fail,E0603
//! use artifact_mcp::model::ArtifactMeta;
//! use artifact_mcp::security::access::OwnedArtifact;
//! fn forge(meta: ArtifactMeta) -> OwnedArtifact {
//!     OwnedArtifact(meta)
//! }
//! ```
//!
//! ```compile_fail,E0616
//! // The wrapped metadata is private, so an authorized value cannot be unwrapped and re-wrapped.
//! use artifact_mcp::security::access::AuthorizedArtifact;
//! fn peek(authorized: AuthorizedArtifact) {
//!     let _leak = authorized.0;
//! }
//! ```
//!
//! ```compile_fail,E0308
//! // A subordinate read cannot be reached with unauthorized metadata.
//! use artifact_mcp::model::ArtifactMeta;
//! use artifact_mcp::ports::ArtifactService;
//! async fn read_before_deciding(service: &dyn ArtifactService, meta: &ArtifactMeta) {
//!     let _body = service.read_body(meta).await;
//! }
//! ```

use crate::{
    artifacts::validation::is_reserved_artifact_id,
    error::AppError,
    model::{ArtifactId, ArtifactMeta, PublisherIdentity, ShareGrant, Viewer},
    ports::ArtifactService,
};

/// Metadata proven readable for one viewer, publisher, or public-share context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedArtifact(ArtifactMeta);

impl AuthorizedArtifact {
    #[must_use]
    pub const fn meta(&self) -> &ArtifactMeta {
        &self.0
    }

    #[must_use]
    pub fn into_meta(self) -> ArtifactMeta {
        self.0
    }
}

/// Metadata proven owned by the current publisher (or an administrator publisher).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedArtifact(ArtifactMeta);

impl OwnedArtifact {
    #[must_use]
    pub const fn meta(&self) -> &ArtifactMeta {
        &self.0
    }

    #[must_use]
    pub fn into_authorized(self) -> AuthorizedArtifact {
        AuthorizedArtifact(self.0)
    }
}

/// `{ ok: false, status: 404, error: "Not found" }`.
///
/// Carried by [`AppError::ConcealedNotFound`], whose `Display` is exactly `Not found`.
pub const NOT_FOUND_MESSAGE: &str = "Not found";

/// `{ ok: false, status: 401, error: "Not signed in" }`.
pub const NOT_SIGNED_IN_MESSAGE: &str = "Not signed in";

/// `{ ok: false, status: 403, error: "Forbidden" }`.
pub const FORBIDDEN_MESSAGE: &str = "Forbidden";

/// `{ ok: false, status: 403, error: "Admins only" }`.
pub const ADMINS_ONLY_MESSAGE: &str = "Admins only";

/// The publisher org that Node treats as the administrator tenant — `auth.org === "admin"`
/// This is also the organization assigned to administrator viewers.
pub const ADMIN_ORG: &str = "admin";

/// Explicit capability refusals. Tenancy denials use [`unknown_artifact_message`] instead.
pub const PUBLISH_PERMISSION_ERROR: &str =
    "Permission denied: reader keys cannot publish artifacts";
pub const READ_PERMISSION_ERROR: &str = "Permission denied: this API key cannot read this artifact";
pub const WRITE_PERMISSION_ERROR: &str =
    "Permission denied: this API key cannot modify this artifact";
pub const DELETE_PERMISSION_ERROR: &str =
    "Permission denied: this API key cannot delete this artifact";

/// `Unknown artifact: ${id}`.
#[must_use]
pub fn unknown_artifact_message(id: &str) -> String {
    format!("Unknown artifact: {id}")
}

/// Whether a denial is allowed to reveal that the artifact exists.
///
/// Mirrors the `{ conceal }` option of `artifactAccess`. Every
/// artifact-scoped human route uses [`Concealment::Conceal`]; [`Concealment::Reveal`] exists
/// only because Node still exports the un-concealed decision and its parity is pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Concealment {
    /// `{ conceal: true }` — foreign and missing answer identically.
    Conceal,
    /// `{ conceal: false }` — the historical `401`/`403` answers.
    Reveal,
}

/// The artifact rows a publisher may list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListScope {
    /// All organizations, for administrator keys.
    All,
    /// The publisher's whole organization, for reader and collaborator keys.
    Org,
    /// Only artifacts owned by the publisher in its organization, for author keys.
    Own,
}

/// Pure access policy. Every function is a total, side-effect-free decision over values the
/// caller already holds; nothing here performs I/O, so a decision can never be skipped by an
/// error path.
pub struct AccessPolicy;

impl AccessPolicy {
    /// JavaScript truthiness for `viewer?.email`: `null`, `undefined` and `""` are all falsy,
    /// so an empty string must not count as signed in.
    fn signed_in_email(viewer: &Viewer) -> Option<&str> {
        viewer
            .email
            .as_ref()
            .map(|email| email.0.as_str())
            .filter(|email| !email.is_empty())
    }

    /// JavaScript truthiness for `viewer.org` in `viewer.org && viewer.org === artifact.org`
    /// An empty org never matches, not even another empty org.
    fn tenant_org(viewer: &Viewer) -> Option<&str> {
        viewer
            .org
            .as_ref()
            .map(|org| org.0.as_str())
            .filter(|org| !org.is_empty())
    }

    /// `!!viewer?.email` — a viewer that Cloudflare Access resolved to a real identity.
    #[must_use]
    pub fn is_signed_in(viewer: &Viewer) -> bool {
        Self::signed_in_email(viewer).is_some()
    }

    /// `viewer.isAdmin || (viewer.org && viewer.org === artifact.org)`.
    ///
    /// Assumes the viewer is signed in; callers go through [`Self::artifact_access`].
    #[must_use]
    pub fn viewer_may_read(viewer: &Viewer, artifact: &ArtifactMeta) -> bool {
        viewer.is_admin || Self::tenant_org(viewer) == Some(artifact.org.0.as_str())
    }

    /// Whether a human may perform owner-scoped artifact mutations.
    ///
    /// Administrators may manage any readable artifact. Other viewers must remain in the
    /// artifact's organization and match its immutable publish-time owner, case-insensitively.
    /// A legacy artifact without `owner_email` is consequently administrator-only.
    #[must_use]
    pub fn viewer_can_manage_artifact(viewer: &Viewer, artifact: &ArtifactMeta) -> bool {
        let Some(viewer_email) = Self::signed_in_email(viewer) else {
            return false;
        };
        if viewer.is_admin {
            return true;
        }
        if Self::tenant_org(viewer) != Some(artifact.org.0.as_str()) {
            return false;
        }
        artifact
            .owner_email
            .as_deref()
            .filter(|owner_email| !owner_email.is_empty())
            .is_some_and(|owner_email| owner_email.eq_ignore_ascii_case(viewer_email))
    }

    /// `artifactAccess(viewer, artifact, { conceal })`.
    ///
    /// The missing-artifact branch is checked **first** and answers `404` under both
    /// concealment modes, which is what makes the concealed foreign answer identical to it.
    ///
    /// # Errors
    ///
    /// * [`AppError::ConcealedNotFound`] — artifact absent, or denied under
    ///   [`Concealment::Conceal`].
    /// * [`AppError::Unauthorized`] — not signed in under [`Concealment::Reveal`].
    /// * [`AppError::Forbidden`] — cross-organization under [`Concealment::Reveal`].
    pub fn artifact_access(
        viewer: &Viewer,
        artifact: Option<&ArtifactMeta>,
        concealment: Concealment,
    ) -> Result<(), AppError> {
        let Some(artifact) = artifact else {
            return Err(AppError::ConcealedNotFound);
        };
        if !Self::is_signed_in(viewer) {
            return Err(match concealment {
                Concealment::Conceal => AppError::ConcealedNotFound,
                Concealment::Reveal => AppError::Unauthorized(NOT_SIGNED_IN_MESSAGE.to_owned()),
            });
        }
        if Self::viewer_may_read(viewer, artifact) {
            return Ok(());
        }
        Err(match concealment {
            Concealment::Conceal => AppError::ConcealedNotFound,
            Concealment::Reveal => AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()),
        })
    }

    /// `concealedArtifactAccess`. **The** decision every artifact-scoped
    /// human route (read *and* mutation) must use, and the only viewer-side constructor of
    /// [`AuthorizedArtifact`].
    ///
    /// Callers pass `None` for a reserved id (see [`Self::lookup_target`]) and for an id that
    /// `ArtifactService::find_meta` did not resolve; both are then indistinguishable from a
    /// foreign artifact and from an unsigned probe.
    ///
    /// # Errors
    ///
    /// Always exactly [`AppError::ConcealedNotFound`] — one status, one body, one cache policy.
    pub fn authorize_viewer(
        viewer: &Viewer,
        artifact: Option<ArtifactMeta>,
    ) -> Result<AuthorizedArtifact, AppError> {
        Self::artifact_access(viewer, artifact.as_ref(), Concealment::Conceal)?;
        artifact.map(AuthorizedArtifact).ok_or({
            // Unreachable: `artifact_access` rejects `None` first. Encoded as the same
            // concealed error rather than an `unwrap`, so the fallible path stays total.
            AppError::ConcealedNotFound
        })
    }

    /// `artifacts.isReserved?.(id) ? null : …`.
    ///
    /// Returns the id to look up, or `None` when the id can never address an artifact. Routes
    /// must feed `None` straight into [`Self::authorize_viewer`] instead of short-circuiting,
    /// so a reserved id costs the same work and yields the same answer as a real miss.
    #[must_use]
    pub fn lookup_target(id: &str) -> Option<ArtifactId> {
        (!is_reserved_artifact_id(id)).then(|| ArtifactId(id.to_owned()))
    }

    /// `adminAccess(viewer)`.
    ///
    /// Deliberately **not** concealed: both denials are `403` about the caller's role, and the
    /// routes that use it (`/settings*`, org registry, webhook admin) carry no artifact id, so
    /// no tenant membership can leak. Note Node answers `403` — not `401` — when unsigned.
    ///
    /// # Errors
    ///
    /// [`AppError::Forbidden`] with `Not signed in` or `Admins only`.
    pub fn admin_access(viewer: &Viewer) -> Result<(), AppError> {
        if !Self::is_signed_in(viewer) {
            return Err(AppError::Forbidden(NOT_SIGNED_IN_MESSAGE.to_owned()));
        }
        if !viewer.is_admin {
            return Err(AppError::Forbidden(ADMINS_ONLY_MESSAGE.to_owned()));
        }
        Ok(())
    }

    /// `auth?.org === "admin"`.
    ///
    /// The publisher admin test is the **org**, exactly as in Node, not
    /// `PublisherIdentity::is_admin`. The authenticator builds the identity from the `api_keys`
    /// row (`client_id`, `org`, `label`) and has no admin flag at all, so the org is the only
    /// value with a Node-defined meaning. Using it keeps the Rust decision fail-closed if the
    /// derived flag were ever set from a wider rule.
    #[must_use]
    pub fn publisher_is_admin(auth: &PublisherIdentity) -> bool {
        auth.org.0 == ADMIN_ORG
    }

    /// Administrators list all artifacts; other keys list by their role.
    #[must_use]
    pub fn publisher_list_scope(publisher: &PublisherIdentity) -> ListScope {
        if Self::publisher_is_admin(publisher) {
            ListScope::All
        } else {
            match publisher.role {
                crate::model::Role::Reader | crate::model::Role::Collaborator => ListScope::Org,
                crate::model::Role::Author => ListScope::Own,
            }
        }
    }

    /// Administrators and author/collaborator keys may publish artifacts.
    #[must_use]
    pub fn publisher_can_publish(publisher: &PublisherIdentity) -> bool {
        Self::publisher_is_admin(publisher) || publisher.role != crate::model::Role::Reader
    }

    fn publisher_owns(auth: &PublisherIdentity, artifact: &ArtifactMeta) -> bool {
        artifact.client_id == auth.client_id && artifact.org == auth.org
    }

    fn publisher_shares_org(auth: &PublisherIdentity, artifact: &ArtifactMeta) -> bool {
        artifact.org == auth.org
    }

    /// Reader/collaborator keys read their organization; author keys read only their own rows.
    #[must_use]
    pub fn publisher_can_read(auth: &PublisherIdentity, artifact: &ArtifactMeta) -> bool {
        Self::publisher_can_read_row(
            Self::publisher_is_admin(auth),
            &auth.org.0,
            &auth.client_id.0,
            auth.role,
            &artifact.org.0,
            &artifact.client_id.0,
        )
    }

    /// Read policy over stored row facts, shared with collection membership checks.
    #[must_use]
    pub fn publisher_can_read_row(
        is_admin: bool,
        publisher_org: &str,
        publisher_client_id: &str,
        role: crate::model::Role,
        artifact_org: &str,
        artifact_client_id: &str,
    ) -> bool {
        is_admin
            || (publisher_org == artifact_org
                && match role {
                    crate::model::Role::Reader | crate::model::Role::Collaborator => true,
                    crate::model::Role::Author => publisher_client_id == artifact_client_id,
                })
    }

    /// Collaborators write throughout their organization; authors write only their own rows.
    #[must_use]
    pub fn publisher_can_write(auth: &PublisherIdentity, artifact: &ArtifactMeta) -> bool {
        if Self::publisher_is_admin(auth) {
            return true;
        }
        Self::publisher_shares_org(auth, artifact)
            && (auth.role == crate::model::Role::Collaborator
                || (auth.role == crate::model::Role::Author
                    && Self::publisher_owns(auth, artifact)))
    }

    /// Delete is irreversible, so non-reader keys may delete only owned rows.
    #[must_use]
    pub fn publisher_can_delete(auth: &PublisherIdentity, artifact: &ArtifactMeta) -> bool {
        Self::publisher_is_admin(auth)
            || (matches!(
                auth.role,
                crate::model::Role::Author | crate::model::Role::Collaborator
            ) && Self::publisher_owns(auth, artifact))
    }

    /// `concealedPublisherRead(auth, artifact, id)`. Invariant 3 on the
    /// MCP read path: an artifact outside the caller's organization reports the identical error
    /// as one that does not exist, so read tools cannot probe tenant membership.
    ///
    /// # Errors
    ///
    /// [`AppError::NotFound`] carrying `Unknown artifact: {id}` for both foreign and missing.
    pub fn authorize_publisher_read(
        auth: &PublisherIdentity,
        artifact: Option<ArtifactMeta>,
        id: &str,
    ) -> Result<OwnedArtifact, AppError> {
        let Some(artifact) = artifact else {
            return Err(AppError::NotFound(unknown_artifact_message(id)));
        };
        if !Self::publisher_is_admin(auth) && !Self::publisher_shares_org(auth, &artifact) {
            return Err(AppError::NotFound(unknown_artifact_message(id)));
        }
        if Self::publisher_can_read(auth, &artifact) {
            Ok(OwnedArtifact(artifact))
        } else {
            Err(AppError::Forbidden(READ_PERMISSION_ERROR.to_owned()))
        }
    }

    /// Publisher write capability with tenant concealment resolved before the role decision.
    ///
    /// # Errors
    ///
    /// * [`AppError::NotFound`] — `Unknown artifact: {id}`.
    /// * [`AppError::Forbidden`] — explicit capability refusal for a same-org artifact.
    pub fn authorize_publisher_write(
        auth: &PublisherIdentity,
        artifact: Option<ArtifactMeta>,
        id: &str,
        _refusal: &str,
    ) -> Result<OwnedArtifact, AppError> {
        let Some(artifact) = artifact else {
            return Err(AppError::NotFound(unknown_artifact_message(id)));
        };
        if !Self::publisher_is_admin(auth) && !Self::publisher_shares_org(auth, &artifact) {
            return Err(AppError::NotFound(unknown_artifact_message(id)));
        }
        if Self::publisher_can_write(auth, &artifact) {
            Ok(OwnedArtifact(artifact))
        } else {
            Err(AppError::Forbidden(WRITE_PERMISSION_ERROR.to_owned()))
        }
    }

    /// Delete authorization is intentionally separate from reversible writes.
    pub fn authorize_publisher_delete(
        auth: &PublisherIdentity,
        artifact: Option<ArtifactMeta>,
        id: &str,
    ) -> Result<OwnedArtifact, AppError> {
        let Some(artifact) = artifact else {
            return Err(AppError::NotFound(unknown_artifact_message(id)));
        };
        if !Self::publisher_is_admin(auth) && !Self::publisher_shares_org(auth, &artifact) {
            return Err(AppError::NotFound(unknown_artifact_message(id)));
        }
        if Self::publisher_can_delete(auth, &artifact) {
            Ok(OwnedArtifact(artifact))
        } else {
            Err(AppError::Forbidden(DELETE_PERMISSION_ERROR.to_owned()))
        }
    }

    /// Require artifact metadata in the same organization as the share before granting access.
    ///
    /// A share grant is scoped to the org it was minted in, so an artifact that has since been
    /// re-tenanted stops matching its old links.
    #[must_use]
    pub fn share_matches(grant: &ShareGrant, artifact: &ArtifactMeta) -> bool {
        grant.org == artifact.org
    }

    /// `sharedArtifactOr404`. The token itself is the authorization
    /// boundary (Cloudflare Access bypasses `/s/*`), so no viewer is resolved here.
    ///
    /// A stale or re-tenanted row is kept indistinguishable from every other invalid token.
    ///
    /// # Errors
    ///
    /// [`AppError::ConcealedNotFound`] — the same answer an unknown, expired, or revoked token
    /// already produces.
    pub fn authorize_share(
        grant: &ShareGrant,
        artifact: Option<ArtifactMeta>,
    ) -> Result<AuthorizedArtifact, AppError> {
        match artifact {
            Some(artifact) if Self::share_matches(grant, &artifact) => {
                Ok(AuthorizedArtifact(artifact))
            }
            _ => Err(AppError::ConcealedNotFound),
        }
    }
}

/// The composed human gate: `artifactForViewer`.
///
/// Reserved check, metadata lookup, then the concealed decision. Nothing subordinate — body
/// bytes, thumbnail, feedback, shares, views, history — is read before the decision, so
/// neither the response nor the work performed can be used as an oracle. Routes that resolve
/// an artifact by id should call this rather than re-assembling the three steps, because the
/// only alternative source of an [`AuthorizedArtifact`] is
/// [`AccessPolicy::authorize_viewer`] itself.
///
/// # Errors
///
/// [`AppError::ConcealedNotFound`] for reserved, missing, foreign, and unsigned alike; or a
/// storage failure surfaced by `find_meta`.
pub async fn resolve_for_viewer(
    artifacts: &dyn ArtifactService,
    viewer: &Viewer,
    id: &str,
) -> Result<AuthorizedArtifact, AppError> {
    let meta = match AccessPolicy::lookup_target(id) {
        Some(target) => artifacts.find_meta(&target).await?,
        None => None,
    };
    AccessPolicy::authorize_viewer(viewer, meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClientId, EmailAddress, OrgId, Timestamp};

    fn meta(org: &str, client_id: &str) -> ArtifactMeta {
        ArtifactMeta {
            id: ArtifactId::from("abc123def456"),
            client_id: ClientId::from(client_id),
            org: OrgId::from(org),
            title: "Concealed".to_owned(),
            description: String::new(),
            bytes: 12,
            created_at: Timestamp("2026-01-01T00:00:00.000Z".to_owned()),
            updated_at: Timestamp("2026-01-01T00:00:00.000Z".to_owned()),
            uploader_label: String::new(),
            owner_email: None,
            is_bundle: false,
            entry: "index.html".to_owned(),
            revision: 1,
            category: String::new(),
            hidden: false,
            body_sha256: "0".repeat(64),
            artifact_key: None,
        }
    }

    fn viewer(email: Option<&str>, org: Option<&str>, is_admin: bool) -> Viewer {
        Viewer {
            email: email.map(EmailAddress::from),
            org: org.map(OrgId::from),
            is_admin,
        }
    }

    #[test]
    fn publisher_listing_and_publish_rights_follow_org_and_role() {
        use crate::model::Role;

        for (role, scope, can_publish) in [
            (Role::Reader, ListScope::Org, false),
            (Role::Collaborator, ListScope::Org, true),
            (Role::Author, ListScope::Own, true),
        ] {
            for org in ["acme", "", "Admin", "admin "] {
                let publisher = PublisherIdentity {
                    client_id: ClientId::from("key"),
                    org: OrgId::from(org),
                    label: String::new(),
                    role,
                    scopes: None,
                };
                assert_eq!(AccessPolicy::publisher_list_scope(&publisher), scope);
                assert_eq!(AccessPolicy::publisher_can_publish(&publisher), can_publish);
            }
            let admin = PublisherIdentity {
                client_id: ClientId::from("key"),
                org: OrgId::from("admin"),
                label: String::new(),
                role,
                scopes: None,
            };
            assert_eq!(AccessPolicy::publisher_list_scope(&admin), ListScope::All);
            assert!(AccessPolicy::publisher_can_publish(&admin));
        }
    }

    #[test]
    fn publisher_row_reads_preserve_collection_policy() {
        use crate::model::Role;

        for (role, owner, allowed) in [
            (Role::Reader, "other", true),
            (Role::Author, "key", true),
            (Role::Author, "other", false),
            (Role::Collaborator, "other", true),
        ] {
            assert_eq!(
                AccessPolicy::publisher_can_read_row(false, "acme", "key", role, "acme", owner),
                allowed
            );
            assert!(!AccessPolicy::publisher_can_read_row(
                false, "acme", "key", role, "globex", owner
            ));
            assert!(AccessPolicy::publisher_can_read_row(
                true, "admin", "key", role, "globex", owner
            ));
        }
    }

    #[test]
    fn empty_strings_are_falsy_like_javascript() {
        let blank_email = viewer(Some(""), Some("acme"), false);
        assert!(!AccessPolicy::is_signed_in(&blank_email));

        let blank_org = viewer(Some("a@acme.test"), Some(""), false);
        assert!(!AccessPolicy::viewer_may_read(&blank_org, &meta("", "k")));
    }

    #[test]
    fn concealed_denials_are_one_error() {
        let inputs = [
            (viewer(None, None, false), Some(meta("acme", "k"))),
            (viewer(Some("b@other.test"), Some("other"), false), None),
            (
                viewer(Some("b@other.test"), Some("other"), false),
                Some(meta("acme", "k")),
            ),
            (viewer(None, None, false), None),
        ];
        for (who, what) in inputs {
            let denied = AccessPolicy::authorize_viewer(&who, what).unwrap_err();
            assert_eq!(denied, AppError::ConcealedNotFound);
            assert_eq!(denied.to_string(), NOT_FOUND_MESSAGE);
        }
    }

    #[test]
    fn reserved_ids_never_reach_a_lookup() {
        for reserved in ["raw", "settings", "mcp", "s", "", "UPPER", "x"] {
            assert!(
                AccessPolicy::lookup_target(reserved).is_none(),
                "{reserved}"
            );
        }
        assert_eq!(
            AccessPolicy::lookup_target("abc123def456"),
            Some(ArtifactId::from("abc123def456"))
        );
    }

    #[test]
    fn admin_re_tenant_revokes_publisher_control() {
        let auth = PublisherIdentity {
            client_id: ClientId::from("acme-key"),
            org: OrgId::from("acme"),
            label: "acme".to_owned(),
            role: crate::model::Role::Author,
            scopes: None,
        };
        assert!(AccessPolicy::publisher_can_read(
            &auth,
            &meta("acme", "acme-key")
        ));
        // Same client_id, artifact moved by an admin: control does not follow.
        assert!(!AccessPolicy::publisher_can_read(
            &auth,
            &meta("globex", "acme-key")
        ));
    }

    #[test]
    fn publisher_admin_is_the_org_not_the_flag() {
        let flagged = PublisherIdentity {
            client_id: ClientId::from("k"),
            org: OrgId::from("acme"),
            label: String::new(),
            role: crate::model::Role::Author,
            scopes: None,
        };
        assert!(!AccessPolicy::publisher_is_admin(&flagged));
        assert!(!AccessPolicy::publisher_can_read(
            &flagged,
            &meta("globex", "other")
        ));
    }
}
