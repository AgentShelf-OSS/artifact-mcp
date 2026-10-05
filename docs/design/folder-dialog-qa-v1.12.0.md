# Folder dialogs and confirmation review

The folder editor follows the approved [wireframe revision 15](https://artifact.neilblackman.dev/0tckejkwv2pa).
Create and edit share a two-column layout with a live folder preview, named color swatches,
optional context, and an explicit cover selector. The production name limit remains 80 characters.
Existing custom colors and covers remain intact when editing.

The admin organization picker uses registered organizations, including organizations with no
artifacts. It does not insert the admin login identity. All-organizations creation requires an
explicit organization choice. Creation from selected artifacts fixes their organization.

Folder deletion uses an in-app confirmation. It names the folder, organization, and artifact
count and states that artifacts remain available. Cancel receives initial focus. Close, Cancel,
and Escape do not write. Request failure retains the confirmation for retry. Cancelling a
nested confirmation preserves the parent editor's draft.

Administration and revision restore use shared in-app confirmations. This includes organization
deletion, publisher-key revocation, discussion credential/destination removal, webhook removal,
and saved-connection disabling/deletion. The existing mutation handlers and CSRF header remain
in place. Draft-loss safeguards in the anchored-comment workflow retain their existing behavior.

## Verification

- Node suite: 517 passed.
- Rust rendering checks and Node oracle comparison: 33 checks passed.
- Focused collection/browser cases: 52 per runtime passed on isolated Node and Rust instances.
- Shared-confirmation cases: 5 per runtime passed, including cancellation, key revocation with
  its CSRF header, and restoring an earlier revision without losing history.
- Keyboard swatches, custom-color/cover persistence, dark-preview contrast, and mobile action
  reachability passed. Collection checks also cover pointer drag, touch pin/dismiss, viewport
  placement after scrolling, folder membership errors, and ribbon order/collapse persistence.
- Desktop and mobile screenshots were captured and inspected in Chrome at 1440 × 1000 and
  375 × 667. The review covered all three library presentations, create/edit/delete dialogs,
  a dark editor, the scrolled mobile preview, and administration confirmation. No uncaught
  browser errors or page overflow occurred in the visual review.

Temporary QA artifacts and their captured PNG thumbnails lived in disposable local data. They
were not published to production. Browser test fixtures clean up their own artifacts and
organizations through the API.

Session screenshots are under `/tmp/artifact-folder-tools-86/`: `create-desktop.png`,
`edit-desktop.png`, `delete-desktop.png`, `create-dark.png`, `create-mobile.png`,
`create-mobile-preview.png`, `admin-confirm-mobile.png`, and the three desktop library views.
These paths are local evidence, not repository assets or durable release attachments.
