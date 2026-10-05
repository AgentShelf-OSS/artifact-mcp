# ADR-0012: Organization collections with viewer presentation preferences

- **Status:** Accepted
- **Date:** 2026-10-05
- **Issues:** #77, #78, #79, #80, #81

## Context

Viewers need topic collections that can contain the same artifact without creating copies. The approved revision-15 wireframe provides Reel Shelf, Contact Sheets, and Gallery Ribbons. These are presentations of one library, not three storage models. Organization and category have existing access and taxonomy meanings and must remain separate from collections.

Shared folders also need a mutation policy. Granting every reader permission to rename or delete another viewer's folders would make shared organization unreliable. Requiring artifact ownership for every membership would prevent a viewer from collecting useful work published by someone else.

## Decision

Store organization-owned collections and reference-only memberships in SQLite. Use one service contract for the three presentations. A membership does not grant artifact access, copy its body, change its category, or create a content revision.

| Action | Administrator | Folder creator in the same organization | Other same-organization viewer |
| --- | --- | --- | --- |
| Read accessible folders and artifacts | Yes | Yes | Yes |
| Create a folder | Any registered organization | Own organization | Own organization |
| Rename, change cover/color, or delete a folder | Yes | Own folders | No |
| Add or remove readable artifacts in a folder | Yes | Own folders | No |
| Change artifact category, visibility, organization, or delete artifact | Existing policy | Existing owner policy | Existing owner policy |

A creator may collect another owner's readable artifact. This changes collection curation, not artifact metadata. Hidden artifact discovery continues to follow the application's current rules. Foreign collection and artifact IDs retain concealed-read behavior. Bulk membership changes validate every target and commit together.

Composite organization foreign keys enforce same-organization membership. Artifact deletion removes references and clears covers. An artifact organization move clears its former collection memberships and covers in the same transaction. Deleting a folder preserves artifacts. Existing organization move confirmation and public-share revocation remain unchanged.

Store display preferences under the verified viewer identity. Selected view, preview size, Grid/List, collection order, and ribbon collapse state belong to that viewer. They do not change shared collection contents. For an administrator, the virtual `all` preference key stores ribbon order and collapse state across the authorized organization union. It is not an organization that can own collections. Do not store gallery preferences in artifact-scoped viewer state.

Use ordered migration 37 and identical SQL in Node and Rust. Keep one application process and the existing server-rendered gallery. Use real production artifact action handlers, not the wireframe's local sample mutations. Membership Undo requires an authorized inverse request. Permanent artifact deletion retains its current explicit confirmation and cannot advertise prototype-only Undo.

## Consequences

The three views stay consistent and can be reviewed separately. Folder creators can organize readable work without receiving permission to modify the underlying artifacts. Organization members can see another creator's collections, but cannot silently change those collections. A future shared editor policy needs a separate explicit decision.

Preference storage requires bounds and stale-ID normalization. Collection queries and thumbnail loading must be bounded. A deleted folder or moved artifact must recover to a valid presentation without revealing another organization's data.
