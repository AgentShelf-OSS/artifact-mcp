# Collection library implementation and local QA

This branch implements #77–#81: persistent organization collections and three library views. The UI calls a collection a folder. Each membership references an existing artifact. It does not copy the artifact, change access, or create a content revision.

- Branch: `feat/collection-library-77`.
- Baseline: `d698a1fd09fa11b4598205e5e11bce403147d715`.
- Approved reference: [wireframe revision 15](https://artifact.neilblackman.dev/0tckejkwv2pa), frozen in `arty-organization-lab.html`.
- Permission and storage decision: [ADR-0012](../adr/0012-organization-collections-and-viewer-presentations.md).
- Schema change: ordered SQLite migration 37, shared by Node and Rust.

## Implemented behavior

Reel Shelf shows folder faces with organization and count inside the folder. Up to three preview corners fan above each face. Hover or focus opens a temporary reel; click or tap pins it. Temporary reels fit above or below the rail according to available viewport space. Pinned reels occupy layout space.

Contact Sheets use one, two, three, or four cover previews. An explicit readable cover appears first. Opening a folder shows its matching artifacts; Back returns to the selected collection presentation.

Gallery Ribbons support expanded previews, collapsed preview fans, grip dragging, and keyboard/touch Move up/down controls. Order, collapse state, view, preview size, and Grid/List preferences belong to the viewer.

All views share search, status, category, authorized organization filters, membership selection, folder creation/editing, and artifact actions. The existing More popup contains Category, administrator Organization, Folders, and confirmed artifact deletion. Failed preference saves restore the saved state. Failed folder loads offer Retry.

Folder creators and administrators curate folders. Other organization members can browse readable contents. Foreign or inaccessible targets remain concealed. Bulk membership writes and their audit records commit together. Artifact deletion and organization moves clear incompatible memberships and covers. Folder deletion preserves artifacts.

## Validation

The disposable Node and Rust preview instances each contain 238 generated artifacts. After test cleanup, Node has 18 folders and Rust has 17. Tests use separate temporary data or their own generated organizations. Production data was not used.

| Check | Result |
| --- | --- |
| Node full suite | 491 passed |
| Rust library and integration suites | 899 passed |
| HTTP conformance, both runtimes | 39 cases each; 78 passed |
| Updated Node portal checks | 16 passed |
| Updated Rust/Node rendering comparison | Passed |
| Browser gallery and collection suite | 46 passed; 23 per runtime, no retries |
| Responsive visual review | 24 combinations; no horizontal page overflow or unexpected browser/request errors |
| Actual touch pin/dismiss | Passed on both runtimes |
| Rust formatting and whitespace checks | Passed |

Visual review covered all three presentations at 1440, 768, 390, and 320 pixels. Additional captures covered a hover reel and dark phone ribbons. Review included long names, real loaded preview images, thumbnail fallbacks, and reduced motion. Compact reel cards measured approximately 251 pixels tall in the desktop fixture.

The browser checks cover atomic membership changes, view restoration, failed preference writes and recovery, failed initial loads and Retry, keyboard pinning, paging, scroll placement, folder picker creation, cover identity, sheet compositions, artifact drops, synchronized favorites, ribbon order/collapse persistence, access boundaries, and focused folder navigation.

The browser harness now removes its generated artifacts and companion organizations through the real APIs. It checks cleanup responses. Semantic label queries are scoped to the toolbar or dialog to avoid expensive page-wide searches across a full library.

## Local review

The worktree is `/mnt/nas/Dev/worktrees/artifact-mcp-collections-77`. The current disposable preview launcher and screenshots are under `/tmp/collections-77`; these are local session artifacts, not deployment configuration.

- Node preview with a local administrator identity: `http://127.0.0.1:3792/?org=designlab`.
- Rust preview with a local administrator identity: `http://127.0.0.1:3793/?org=designlab`.
- Direct test targets: Node `3790`, Rust `3791`. Browser tests supply their own identities; do not target the preview proxies for signed-out tests.
- Captures: `/tmp/collections-77/final-{node,rust}-{reel,sheets,ribbons}-{1440,768,390,320}.png`.

The preview services bind to loopback. Forward a preview port over SSH when reviewing from another computer. The generated credentials stay in private files in the disposable data directory.

Useful checks from this worktree:

```sh
npm test
cargo test --lib --tests
cargo fmt --all -- --check
RUST_ARTIFACT_MCP_BIN=/tmp/collections-77/rust-target/debug/artifact-mcp node conformance/runner.mjs --impl both
```

Browser command from `playwright/`, with both disposable services running:

```sh
PW_NODE_URL=http://127.0.0.1:3790 PW_RUST_URL=http://127.0.0.1:3791 PW_ADMIN_EMAIL=admin@example.test PW_USE_BUNDLED_CHROMIUM=1 npx playwright test -c playwright.config.mjs tests/01-gallery.spec.mjs tests/10-collections.spec.mjs
```

No merge or deployment was performed. This branch starts from committed master. The original checkout's separate scene-capture changes remain untouched; rerun the viewer integration checks if those changes land before this branch.
