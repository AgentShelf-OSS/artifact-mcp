# Web interface performance exploration — 2026-10-05
This report covers the earlier five-change experiment. See the [four-change inline release QA](web-interface-inline-release-20261005.md) for the current v1.12.0-based candidate.

All five experiments are implemented on `perf/web-interface-prod-20261005`. Search handlers and anchor updates do less work. Cached visits transfer less shared code. Large galleries still need a limit on rendered cards. At 250 artifacts, the faster handler did not produce a clear improvement in the time to render search results.

The [follow-up QA](web-interface-qa-20261005.md) found a release blocker in shared asset delivery. The current production binary returns `404` for candidate asset URLs after rollback. Hold promotion of this complete branch until asset compatibility is resolved.

The worktree is `/mnt/nas/Dev/worktrees/artifact-mcp-web-perf-20261005`. Production was inspected read-only. No deployment, production data change, or change to the original development checkout was made.

[Anthropic's article](https://claude.dev/blog/how-we-made-claude-ai-faster/) argues for measuring specific user journeys, checking that work counts correlate with elapsed time, and protecting improvements with regression checks. This exploration applies that approach to gallery search, anchored feedback, viewer startup, shared assets, and gallery growth. It measures handler time separately from rendering time so a smaller JavaScript count cannot stand in for a faster user experience.

## Production baseline and test method

The baseline is the running production build, rather than the dirty development checkout:

| Item | Value |
| --- | --- |
| Production commit | `13be8f7b4aaad0a29360a658b258969bb9ccba4d` |
| Version / database schema | `v1.11.2` / `37` |
| Copied production binary SHA-256 | `02b30261b6a72f3a3732da5ec22a6f2a9104a885dffc27aa723b1f11375bd94f` |
| Candidate binary SHA-256 | `4e9c638c7f27a96190b17c4b75e4f9d9b6e59c976b150419e3c7ad7a13a2222a` |
| Browser | Chromium `146.0.7680.71` |
| CPU setting | DevTools 4× slowdown |
| Viewports | Gallery: 1440 × 900; viewer: 1440 × 900 and 390 × 844 |

The copied binary's hash matched the running binary and deployment metadata. Baseline source was frozen from the same commit. Both builds ran on loopback with separate temporary databases, generated credentials, and synthetic artifacts. No production content or credentials were copied.

The live aggregate was 247 artifacts across seven organizations, with no collections and 14 feedback records. The 250-artifact fixture approximates that library size, using one synthetic organization and no collections. The 1,000- and 3,000-artifact stress fixtures have 12 collections and 20% overlapping memberships. The viewer has 100 synthetic anchored comments, well above the live feedback count. Each artifact contains about 25 kB of local HTML; thumbnails are 1 × 1 PNGs.

Baseline and candidate cases ran serially, after builds and functional checks had finished. The 250-artifact local comparison used three fresh browser contexts, each with a cold visit and a warm reload. Five queries per visit give 30 search samples per build. Network and stress cases used one cold/warm pair, giving ten search samples. The network probe used 60 ms latency and 10 Mbps download throughput.

**Handler time** is the synchronous duration of dispatching a search input event, including collection listeners. **Rendering proxy** is elapsed time through two animation-frame callbacks after that event. It includes the intervening browser work, but is not a field interaction-latency metric or proof of pixel presentation. **Gallery ready** ends after the scripts, collection projection for stress fixtures, and two animation frames finish. Medians and the nearest-rank P95 describe these lab samples; they are not production percentiles.

Transfer totals use Chromium Resource Timing for the page HTML plus `/assets/` resources. They exclude thumbnails, iframe HTML, and API traffic. Loopback responses are uncompressed. These byte counts do not predict Cloudflare's compressed transfer sizes.

The compact measurements are in [web-interface-20261005.json](web-interface-20261005.json). Full logs, fixtures, and the disposable benchmark are under `/tmp/artifact-web-perf-20261005-b0hjaznt`.

## The five experiments

| Item | Implemented change | Finding | Recommendation |
| --- | --- | --- | --- |
| 1. Gallery search | Cache sort metadata; sort only when required; write visibility only when it changes; debounce URL/session persistence and flush on page exit. | At 250 artifacts, median handler time fell from 191.9 ms to 6.35 ms. Rendering remained about 340 ms. | Keep the reduced work. Measure result rendering as a separate target. |
| 2. Anchored feedback | Cache normalized anchor payloads; combine parent scroll/resize work within animation frames; combine iframe scroll/resize geometry updates. | A burst of 100 events produced one message instead of 100, on both sides of the iframe. | Keep both schedulers and their message-count checks. |
| 3. Optional viewer work | Load discussion status on the first Details open; cache a successful result; discover voices during an idle callback with a two-second deadline. | Discussion requests before Details fell from one to zero. Voice capability checks and reader behavior pass. | Keep the deferral. Preserve the immediate message listeners and reader control space. |
| 4. Shared asset delivery | Serve fixed trusted CSS/JS files at digest URLs with immutable caching and strong validators. Keep private configuration and theme startup in HTML. | Warm HTML plus shared-asset transfer fell 13.7% for the gallery and 52.0% for the viewer. Cold transfer rose slightly. | Keep for repeated visits; assess cold-load behavior separately before promotion. |
| 5. Gallery/collection growth | Reuse card and membership indexes; skip unchanged card enhancement and inactive All-view collection markup. Probe larger libraries and a CSS rendering experiment. | At 1,000 artifacts, All-view handler time fell from 9.63 seconds to 24.7 ms. About 84,000 DOM nodes remain. | Keep the indexes. Follow with bounded card rendering and server-side filtering. |

Production already avoided moving cards when their DOM order was unchanged. The search gain comes from avoiding repeated sorting and related collection work; it does not come from removing a pre-existing unconditional card reorder.

## Gallery measurements

| 250 artifacts, local connection | Production | Candidate |
| --- | ---: | ---: |
| Handler median | 191.90 ms | 6.35 ms |
| Handler P95 | 408.60 ms | 10.50 ms |
| Rendering proxy median | 346.30 ms | 340.35 ms |
| Rendering proxy P95 | 686.90 ms | 747.80 ms |
| Cold gallery-ready median | 2,356.62 ms | 2,108.50 ms |
| Warm gallery-ready median | 2,058.76 ms | 2,281.09 ms |
| DOM nodes | 21,211 | 21,211 |

DOM means the document object model: the elements that the browser must style and lay out. The unchanged DOM count explains why removing JavaScript work does not remove the layout cost. Candidate counters showed one sort across seven filter passes and one initial enhancement per card. Browser checks confirm that repeated searches keep the current sort and do not repeat card enhancement.

The network probe showed the same limit. Handler medians were 181.95 ms and 7.15 ms. Rendering proxy medians were 319.50 ms and 359.80 ms. Cold readiness was 2,759.78 ms and 2,747.92 ms; warm readiness was 2,584.72 ms and 2,667.48 ms. These small samples do not establish a faster 250-card load or result render.

| 1,000 artifacts, 12 collections | Production All | Candidate All |
| --- | ---: | ---: |
| Handler median | 9,626.65 ms | 24.70 ms |
| Rendering proxy median | 10,379.45 ms | 1,436.35 ms |
| Cold gallery ready | 28,242.09 ms | 9,970.20 ms |
| Warm gallery ready | 27,135.81 ms | 8,660.68 ms |
| DOM nodes | 84,404 | 84,210 |

The candidate still took 3.0–3.5 seconds through two animation frames when clearing the search to show all 1,000 cards. This is a remaining browser-layout cost, even though the handler takes tens of milliseconds.

| Candidate collection view at 1,000 artifacts | Handler median | Rendering proxy median | DOM nodes after clearing search |
| --- | ---: | ---: | ---: |
| All | 24.70 ms | 1,436.35 ms | 84,210 |
| Reel | 863.05 ms | 1,404.60 ms | 84,404 |
| Sheets | 46.30 ms | 1,153.65 ms | 84,403 |
| Ribbons | 92.15 ms | 1,192.05 ms | 87,583 |

Reel still performs substantial synchronous work. Collection renders rebuild their surface and scan membership lists. Ribbons also retain the canonical card grid while adding 36 visible card copies in this fixture. The indexes improve lookup cost, but they do not bound the rendered library.

The production Reel probe failed its 30-second script-readiness wait and exited after 105 seconds overall. It has no valid search timing comparison. Failed probes must not be treated as measured speedups.

At 3,000 artifacts, the candidate created **252,210 DOM nodes** and returned about **15.5 MB of HTML**. Cold readiness was 39.0 seconds and warm readiness was 33.7 seconds. Median handler time was 84.7 ms, while the rendering proxy was 5.55 seconds. Clearing the search took 12.5–12.8 seconds through two animation frames. The current full-grid design is unsuitable for this size, despite the reduced JavaScript work.

The first production 3,000-card probe hit a debugger response-body cache limit. The harness was changed to read the HTML size from navigation metadata. A retry missed the script-readiness wait. A final diagnostic reached its 180-second limit, with no browser errors recorded before cleanup. There is no valid production search median for this size, so no numerical 3,000-card speedup is claimed.

## Viewer work and shared assets

The scroll probe dispatched 100 events synchronously. It proves that events are combined; it does not measure sustained frame rate during physical scrolling.

| Viewer burst, local connection | Production desktop / phone | Candidate desktop / phone |
| --- | ---: | ---: |
| Parent-to-frame repaint messages | 100 / 100 | 1 / 1 |
| Parent synchronous dispatch work | 17.8 / 16.0 ms | 0.2 / 0.8 ms |
| Frame-to-parent position messages | 100 / 100 | 1 / 1 |
| Frame synchronous dispatch work | 80.3 / 78.6 ms | 0.3 / 0.3 ms |
| Discussion GETs before Details | 1 / 1 | 0 / 0 |
| Total discussion GETs after opening Details twice | 1 / 1 | 1 / 1 |

Voice discovery still occurs without a click. It moves to an idle period, rather than disappearing from startup. In the local desktop probe its request began around 537 ms instead of 169 ms. The Listen control reserves its place immediately, stays disabled during discovery, enables when voices are available, and hides when the capability is disabled or fails. The reader matrix covers playback, scopes, resume, rewind, highlights, and prefetch.

| HTML plus shared-asset transfer | Production | Candidate | Change |
| --- | ---: | ---: | ---: |
| Gallery cold | 1,508,588 bytes | 1,514,343 bytes | +0.4% |
| Gallery warm | 1,508,588 bytes | 1,302,321 bytes | −13.7% |
| Viewer cold | 378,443 bytes | 382,238 bytes | +1.0% |
| Viewer warm | 378,443 bytes | 181,579 bytes | −52.0% |

The gallery loads six shared files; this viewer loads three. Their URLs use the first 16 hexadecimal characters of SHA-256. Both runtimes serve only exact allowlisted assets, use `public, max-age=31536000, immutable`, and provide a full SHA-256 ETag. An ETag is a response validator that lets the client ask whether the content changed. Conditional requests return `304` with an empty body. Unknown files, wrong digests, and traversal paths fail closed. Viewer identity, permissions, feedback, and configuration remain in the private HTML.

Moving the scripts exposed two ordering problems. A raw artifact can send its initial state or anchor message before the deferred shell starts. A small inline collector now retains only a normalized `state:hello` from the current artifact frame, then hands it to the existing broker. An `anchor:hello` request lets the shell recover anchor readiness after its listener binds and after frame loads. Source checks remain in place. A browser test deliberately holds the shell script until the artifact has loaded and checks both state readiness and the saved anchor without scrolling.

The candidate showed all 100 saved markers before a scroll event. The copied production build showed none at that checkpoint and painted them after the scroll burst. This is a startup-order finding from the synthetic fixture, rather than a claim that every production artifact has missing markers.

The desktop and phone screenshots were inspected. The Details panel and controls retain the existing layout, with no horizontal overflow. The many brown anchor regions come from the stress fixture. Compare [production desktop](evidence/production-viewer-desktop.png), [candidate desktop](evidence/candidate-viewer-desktop.png), [production phone](evidence/production-viewer-phone.png), and [candidate phone](evidence/candidate-viewer-phone.png).

## Next rendering change

| Option | What it reduces | Constraint | Assessment |
| --- | --- | --- | --- |
| Server pagination with bounded card pages | HTML size, DOM count, initial layout, and per-search work | Search, sort, filters, and counts must operate across the full matching library. Selection must use artifact IDs across pages. | Recommended next implementation. |
| Rendering only visible cards | DOM and layout while keeping continuous scrolling | Requires explicit handling of focus, menus, selection, scroll restoration, and the three collection views. Sending every record still leaves a payload cost. | Useful after the data/query boundary is defined. |
| CSS `content-visibility` | Browser work for offscreen cards | Retains HTML and DOM size. Menus, drag/drop, card geometry, keyboard navigation, and print need review. | Diagnostic only in this branch. |

The CSS probe added `content-visibility:auto; contain-intrinsic-size:auto 450px` to canonical cards after the page loaded. At 1,000 artifacts, its rendering proxy median was 464.15 ms, compared with 1,436.35 ms for the candidate without this rule. Clearing the search took about 444–470 ms. This is a promising small follow-up to validate against the full interaction and layout matrix. The rule is not included in the product stylesheet. Its post-load injection cannot establish a startup benefit, and it leaves the large HTML response and DOM in place.

Retain the current cards and actions, but limit how many cards a page creates. Apply query, organization, category, status, collection scope, and sort before taking a bounded page on both runtimes. Keep aggregate counts and collection membership independent of the current card window. Avoid defining “all matching artifacts” as only the cards currently rendered.

## Validation and promotion status

- Node suite: **513 passed**. The final Node renderer correction also passed its focused **81-test** check.
- Rust native integration target: **653 passed**, with no ignored cases.
- Full browser parity suite: **188 passed, two existing Node-only skips**, no failures. The skips cover native discussion-management behavior that Node does not support.
- Reader matrix: **14 executions passed, none skipped**.
- The first reader-matrix attempt had one rewind-tolerance timing miss. The unchanged assertions and source passed a full rerun. No tolerance was widened.
- Formatting, JavaScript syntax, and diff checks passed.

The browser suite adds checks for immutable-asset validation, stable search work, deferred discussion loading, voice capability states, and late-script anchor/state startup. Existing sandbox, navigation, state, data, feedback, cast, and reader assertions were retained. Shared bridge bytes remain aligned between Node and Rust.

To repeat a local gallery probe while the disposable lab is available:

```sh
node /tmp/artifact-web-perf-20261005-b0hjaznt/benchmark.cjs \
  --source=/mnt/nas/Dev/worktrees/artifact-mcp-web-perf-20261005 \
  --binary=/tmp/artifact-web-perf-20261005-b0hjaznt/cargo-target/release/artifact-mcp \
  --label=candidate-repro \
  --output=/tmp/artifact-web-perf-20261005-b0hjaznt/candidate-repro.json \
  --counts=250 --repeats=3 --views=all --cpu=4 --skip-viewer=1
```

Use the frozen `baseline-source` directory and `production-artifact-mcp` binary under the same lab directory for the baseline. `--network=1` enables the network probe; `--counts=1000 --repeats=1` enables the collection stress fixture. The helper creates its own loopback server, database, and mock speech worker, then closes them. The committed browser regression checks are durable; the scratch lab is temporary.

This is an experiment branch. No production rollout or production latency claim is included. The strongest next steps are to keep the reduced search/scroll work, retain optional-work deferral, review asset delivery with its startup fixes, and then reduce the number of cards created by the gallery.
