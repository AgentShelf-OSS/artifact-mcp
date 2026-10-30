# Frontend maintenance

This guide describes the current Artifact MCP UI contract. Keep the Rust renderer, the shared assets, and browser behavior aligned when changing a surface.

## Design and layout contract

The visual language is a restrained slate / paper / copper system: cool slate application chrome, warm paper panels, and copper for focus, active, and destructive-adjacent emphasis. Keep controls compact and information-dense, with clear monospace metadata and serif artifact titles.

- The Gallery is a top-toolbar library with no left filter rail. It uses three columns by default, two below `1120px`, and one below `760px`; cards are equal-height in grid mode. The layout toggle supports grid and list modes.
- The full artifact view is a focused viewer shell. It does not include the global Gallery masthead; the shell's Back control returns to the library. Keep artifact content in the sandboxed iframe and keep viewer actions in the trusted shell/inspector.
- Settings, not-found, sign-in, and access-retry pages use the same saved-theme foundation. MCP review is a trusted local review surface and uses the same slate/paper/copper vocabulary.
- Interactive targets must remain usable on narrow screens: the mobile rules use at least 44px controls, no horizontal overflow, visible `:focus-visible` treatment, reduced-motion handling, and forced-colors fallbacks.

## Renderer and state boundaries

The Rust server renders pages through Askama templates and the native renderer. A change to HTML structure, inline assets, escaping, or sandbox attributes changes the HTTP conformance goldens and the Playwright journeys. Review those diffs as contract changes.

Gallery and viewer chrome include their CSS and JavaScript in the page HTML. Keep viewer identity, permissions, feedback, and artifact configuration in the private page HTML. Keep theme startup before paint. Changes to an embedded Rust asset require a new binary build.

The performance experiment tested separate cached application files. Follow-up QA found that pending file requests would fail after a rollback to the current production binary. The four-improvement release keeps files inline. See [the follow-up QA](performance/web-interface-qa-20261005.md).

The viewer sends `anchor:hello` after its message listener is ready and after iframe loads. The sandboxed anchor bridge replies with its page identity and current positions. Preserve the source checks on both sides so a late application script can recover readiness without accepting messages from other frames.

The small inline `assets/viewer-boot.js` bootstrap retains one early `state:hello` from the artifact frame. The shell removes that listener and replays the normalized handshake through the existing state broker. It queues no state reads or mutations and starts no requests for artifacts that do not use state. Keep this bootstrap before the iframe.


Gallery sort metadata and collection indexes assume that canonical cards stay in place until navigation, apart from deletion and sorting. A new flow that inserts or replaces cards must invalidate those caches and the card-enhancement signatures. Preserve search checks that reject repeated sorting and repeated enhancement when the card set and collection membership have not changed.

The current browser state keys are:

- `localStorage["artifact-theme"]` — the selected light/dark theme, bootstrapped before paint.
- `localStorage["artifact-layout"]` — Gallery grid/list preference.
- `sessionStorage["artifact-library-state"]` — Gallery filters, sort, query, and scroll position.
- `sessionStorage["artifact-library-return"]` — one-shot marker used when returning from a library-opened artifact. The bfcache `pageshow` path clears a stale marker.

When changing these keys, update both the URL/session restoration behavior and the corresponding Gallery tests. Direct viewer visits should not inherit a library return snapshot.

## Security boundaries

Published HTML is untrusted. Keep the viewer iframe sandbox as `allow-scripts allow-popups allow-forms allow-modals`, without `allow-same-origin`. Raw delivery also receives the CSP sandbox policy. Preserve relative bundle-path normalization and traversal guards.

All request-derived values in templates must be escaped. Keep CSRF/mutation protections and the `x-artifact-mutation` signaling path intact; UI styling must not weaken route authorization, tenant scoping, or concealed-read behavior. The trusted shell must not expose secrets to artifact content. The MCP review asset is intentionally self-contained and declares a restrictive CSP; review changes against `assets/mcp-review-app.html` before adding a dependency or external resource.

## Ownership and file map

- Gallery markup: `templates/gallery.html`; composition and projections: `src/render/portal.rs` and `src/render/view_models.rs`.
- Gallery behavior and layout: `assets/portal/`, `assets/portal.css`. The gallery script is one closure split into numbered parts. `assets/portal/parts.txt` lists their order; `src/render/portal.rs` joins them with `concat!`, and browser tests use `readParts("assets/portal")`. Add each new part to both lists in the same position. A unit test checks the lists and assembled script.
- Full viewer shell and feedback inspector: `templates/artifact-shell.html`, `assets/shell/`, `assets/shell.css`, with shell projections in `src/render/portal.rs`.
  The shell script is one closure split by responsibility into numbered parts (brokers, reactions and inspector, the Listen reader, comments, history). `assets/shell/parts.txt` lists the order; `src/render/portal/templates.rs` joins the parts with `concat!` in that order, and `readShell()` in `browser-tests/js/harness.js` does the same for the browser tests. A part alone is not valid JavaScript. To add a part, add the file, list it in `parts.txt`, and add its `include_str!` at the same position; a unit test fails when the two lists differ.
- Administration: `templates/settings.html`, `src/render/settings.rs`, `assets/settings.js`, `assets/settings.css`.
- Access states: `templates/not-found.html`, `templates/not-signed-in.html`, `templates/access-retry.html` and their matching `assets/not-found.css`, `assets/not-signed-in.css`, `assets/access-retry.css`.
- Trusted MCP review surface: `assets/mcp-review-app.html`.
- Anchored comment interactions are owned by PBI-086. Coordinate changes to marker, composer, inspector, and feedback projection behavior with that implementation rather than duplicating it in Gallery code.

## Focused verification

From the repository root:

```sh
export CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell-linux64/chrome-headless-shell
cargo test --manifest-path browser-tests/Cargo.toml --test js -- gallery-library-layout admin-access-visual-contract portal
cargo test --test native u15_render
```

The browser-asset tests run in Chrome for Testing `chrome-headless-shell`, driven from Rust. Use the version pinned in `preview-renderer/Dockerfile`. See [`browser-tests/README.md`](../browser-tests/README.md).

For browser behavior, run the end-to-end journeys in `browser-tests/tests/e2e`. They start a disposable Rust server with its own data directory; see [`browser-tests/README.md`](../browser-tests/README.md). Run the focused suites:

```sh
cargo build --release --locked
ARTIFACT_MCP_BIN=$PWD/target/release/artifact-mcp \
  cargo test --manifest-path browser-tests/Cargo.toml --locked --test e2e -- 01-gallery 02-viewer 08-settings
```

Use separate throwaway data directories and disposable test keys. Never point the harness at production data. For a broad contract check, run `cargo test --manifest-path browser-tests/Cargo.toml --locked` and `cargo test --all-targets --locked` from the repository root.

## Screenshot refresh

The canonical visual evidence is in [`docs/screenshots/`](screenshots/). Refresh screenshots when a layout, breakpoint, theme, access state, settings surface, viewer shell, or comment interaction changes—not for copy-only or test-only edits. Capture the same named states and viewport/theme combinations used by the browser journeys in `browser-tests/tests/e2e/`, review desktop/tablet/phone and light/dark output, then replace only the affected images. Keep screenshots free of credentials, real artifact content, and production identifiers. Record the validating command and affected state in the change description.
