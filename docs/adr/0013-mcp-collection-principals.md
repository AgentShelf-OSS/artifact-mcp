# ADR-0013: MCP collection service principals

- **Status:** Accepted
- **Date:** 2026-10-05
- **Issue:** #86
- **Related ADR:** [ADR-0012](0012-organization-collections-and-viewer-presentations.md)

## Context

Organization collections are shared references to artifacts. The browser creates and curates
them as a signed-in human viewer, whose verified email is already the browser ownership key.
MCP requests authenticate as publisher credentials instead. API keys and OAuth tokens do not
provide one common, verified human email, and an API key's optional `owner_email` describes a
human association for key administration rather than the actor making an MCP request.

MCP therefore must not impersonate an API-key owner email. Doing so would make a service action
look like a human action, make ownership change when key administration changes, and leave OAuth
without an equivalent identity. The collection owner must remain stable across secret rotation and
must not collide when two OAuth issuers use the same client ID.

## Decision

MCP collection mutations use a service principal derived from the authenticated publisher
identity. The principal is part of the collection actor passed to the collection service; no new
authentication framework is introduced.

The persisted `collections.created_by` value remains the principal identifier for compatibility.
Ordered migration 038 adds `created_by_kind TEXT NOT NULL DEFAULT 'email'`. Existing rows retain
their current `created_by` value and are interpreted as `email`. New rows use these values:

| Actor | `created_by_kind` | `created_by` |
| --- | --- | --- |
| Browser viewer | `email` | normalized verified email |
| API-key MCP principal | `api_key` | `client_id` |
| OAuth MCP principal | `oauth` | canonical compact JSON `[issuer, client_id]` |

The OAuth JSON array has a fixed two-element order, uses compact JSON, and is compared as an exact
stored value. This prevents a client ID collision between issuers. The client ID comes from verified token claims. The issuer comes from the OAuth configuration
against which the token was verified. Neither value is accepted from tool arguments.

The optional API-key `owner_email` is not used for MCP collection ownership or authorization.
Secret rotation that preserves a `client_id` preserves the service principal. Creating a new
`client_id` creates a new principal and does not inherit folders from the old one. An administrator
can curate those folders through the existing administrative path if needed.

### Tool surface and authorization

The MCP surface consists of seven organization-scoped tools:

1. `list_collections`
2. `get_collection`
3. `create_collection`
4. `update_collection`
5. `delete_collection`
6. `add_artifacts_to_collection`
7. `remove_artifacts_from_collection`

`artifacts:read` is required for collection and membership reads. `artifacts:publish` is required
for all collection mutations. The existing organization pinning rules still apply: a non-admin
principal can address only its authenticated organization, while an administrator may select an
authorized organization through the existing `org` argument rules.

Reader principals are read-only. Author principals may mutate only collections whose persisted
principal exactly matches their service principal. Collaborator principals may mutate collections
under the same ownership rule; they do not gain a general organization-wide collection editor
grant. Administrators may manage collections in authorized organizations. Browser email ownership
rules remain unchanged, and a browser human cannot edit a service-principal collection unless the
viewer is an administrator.

| Operation | `artifacts:read` | `artifacts:publish` | Principal rule |
| --- | --- | --- | --- |
| List or inspect collections | Required | Not required | Same authenticated organization |
| Create collection | Not required | Required | Creates ownership for the service principal |
| Rename, edit, or delete | Not required | Required | Exact creator principal or administrator |
| Add or remove members | Not required | Required | Exact creator principal or administrator |

Every artifact member and cover is checked with the existing publisher read authorization before
it is returned or written. Authorization occurs on metadata before any body, revision, bundle, or
thumbnail read. Missing, foreign, or concealed artifacts use the existing concealed-not-found
result. Collection membership never widens artifact access.

Collection mutations reuse the existing publisher mutation audit context and record the existing
collection operation classes: `collection.create`, `collection.update`, `collection.delete`,
`collection.membership.add`, and `collection.membership.remove`. The audit actor is the verified
publisher client identity; secrets, bearer tokens, and owner-email associations are not recorded.

### Bounded reads

Collection and membership listings use bounded offset cursors. Cursors are opaque base64url values
encoding this exact ten-element tuple:

`[1, toolName, authenticatedOrg, targetOrg, principalKind, principalId, role, idOrEmpty, limit, offset]`

Collections sort by UTF-8 name and then UTF-8 ID. Memberships first apply artifact visibility and
then sort by UTF-8 title and then UTF-8 artifact ID. The server decodes and validates the cursor,
re-applies the organization, principal, role, and request bounds on every read, and rejects a
cursor whose tuple does not match the current request. A cursor is pagination state, not an
authentication mechanism. Because this is offset pagination, concurrent membership changes may
shift later pages; callers can restart from the first page when they need a stable snapshot.

Collection and membership pages default to 25 rows. Callers may request 1 to 100 rows.
Collection offsets are bounded to 200 and membership offsets to 1,000. Artifact authorization is applied to
each returned member, so a collection may have fewer visible members than its stored membership
count.

## Consequences

Service-created folders have durable, auditable ownership that does not depend on a human email or
on a secret value. API-key secret rotation is safe when the client ID remains stable. Replacing a
credential with a new client ID intentionally requires administrative curation; it does not silently
transfer ownership.

The collection persistence layer must support both legacy email rows and typed service-principal
rows. Browser projection and editability remain compatible with existing collections. MCP tool
definitions and conformance fixtures must describe the seven tools, their scope requirements,
organization pinning, cursor bounds, and concealed artifact behavior.

No behavior is implied for tools or transfer workflows until their implementation and tests land.
This ADR defines the ownership and authorization contract for issue #86.
