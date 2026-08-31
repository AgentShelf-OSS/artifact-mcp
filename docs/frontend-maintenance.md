# Frontend maintenance

This guide describes the current Artifact MCP UI contract. Keep the Node and Rust renderers, their shared assets, and browser behavior aligned when changing a surface.

## Design and layout contract

The visual language is a restrained slate / paper / copper system: cool slate application chrome, warm paper panels, and copper for focus, active, and destructive-adjacent emphasis. Keep controls compact and information-dense, with clear monospace metadata and serif artifact titles.

- The Gallery is a top-toolbar library with no left filter rail. It uses three columns by default, two below `1120px`, and one below `760px`; cards are equal-height in grid mode. The layout toggle supports grid and list modes.
- The full artifact view is a focused viewer shell. It does not include the global Gallery masthead; the shell's Back control returns to the library. Keep artifact content in the sandboxed iframe and keep viewer actions in the trusted shell/inspector.
- Settings, not-found, sign-in, and access-retry pages use the same saved-theme foundation. MCP review is a trusted local review surface and uses the same slate/paper/copper vocabulary.
- Interactive targets must remain usable on narrow screens: the mobile rules use at least 44px controls, no horizontal overflow, visible `:focus-visible` treatment, reduced-motion handling, and forced-colors fallbacks.

## Renderer and state boundaries

Node is the reference server-rendered implementation; Rust mirrors it through Askama templates and the native renderer. Changes to HTML structure, inline assets, escaping, or sandbox attributes require parity checks in both runtimes. Do not introduce a renderer-only visual behavior.

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

- Gallery markup: `templates/gallery.html`; Node/Rust composition and projections: `lib/portal.js`, `src/render/portal.rs`, and `templates/gallery.html`.
- Gallery behavior and layout: `assets/portal.js`, `assets/portal.css`.
- Full viewer shell and feedback inspector: `templates/artifact-shell.html`, `assets/shell.js`, `assets/shell.css`, with Node/Rust shell projections in `lib/portal.js` and `src/render/portal.rs`.
- Administration: `templates/settings.html`, `lib/settings.js`, `assets/settings.js`, `assets/settings.css`.
- Access states: `templates/not-found.html`, `templates/not-signed-in.html`, `templates/access-retry.html` and their matching `assets/not-found.css`, `assets/not-signed-in.css`, `assets/access-retry.css`.
- Trusted MCP review surface: `assets/mcp-review-app.html`.
- Anchored comment interactions are owned by PBI-086. Coordinate changes to marker, composer, inspector, and feedback projection behavior with that implementation rather than duplicating it in Gallery code.

## Focused verification

From the repository root:

```sh
node --test test/gallery-library-layout.test.js test/admin-access-visual-contract.test.js test/portal.test.js test/feedback.test.js
cargo test --test native u15_render
```

For browser behavior, use the isolated two-runtime Playwright harness described in [`playwright/README.md`](../playwright/README.md). Run the focused suites after starting disposable Node and Rust instances:

```sh
cd playwright
npx playwright test tests/01-gallery.spec.mjs tests/02-viewer.spec.mjs tests/08-settings.spec.mjs -c playwright.config.mjs
```

Use separate throwaway data directories and disposable test keys. Never point the harness at production data. For a broad contract check, run `npm test` and `cargo test --all-targets --locked` from the repository root.

## Screenshot refresh

The canonical visual evidence is in [`docs/screenshots/`](screenshots/). Refresh screenshots when a layout, breakpoint, theme, access state, settings surface, viewer shell, or comment interaction changes—not for copy-only or test-only edits. Capture the same named states and viewport/theme combinations used by the Playwright specs, review desktop/tablet/phone and light/dark output, then replace only the affected images. Keep screenshots free of credentials, real artifact content, and production identifiers. Record the validating command and affected state in the change description.
