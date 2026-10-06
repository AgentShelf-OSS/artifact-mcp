# Web interface QA for the inline release

The four changes to gallery search, comment positions, optional viewer work, and folder rendering
passed local QA. The shared asset caching change is excluded. Styles and scripts remain inside
each page, so a rollback does not need the candidate's external asset files.

The candidate starts from production v1.12.0, commit
`d9d10401b8c777a72e5fd4e57a294f2877ba6d6d`. It preserves schema 38, organization folder tools,
service-principal ownership, folder dialogs, and action grants. The original development checkout
and production service were not changed by this QA.

## Candidate and evidence

The application changes were tested at `cb53977`. The frozen native test binary has SHA-256
`17ee0f4247af88834655aeb28f16ac538dbf4b2937c284ebc1f7942588daffe1`.
The branch was then rebased onto current master `9fc32c7`, producing `e2edc8f`.
A diff confirmed identical application assets, Rust sources, templates, Node rendering code,
and browser tests before and after that rebase. The upstream changes were documentation only.

Release preparation sets the package version and MCP server version to 1.12.1. It updates only
the current version goldens. Historical fixture metadata is unchanged. Release metadata checks
are separate from the application checks below.

The v1.12.1 native build passed with `cargo build --release --locked`. Its local SHA-256 is
`bdcfa7a6d600b28ebbf4bf13104e338f95c196e3aacca4db8729b75d7028c1cf`.
After the version update, all 45 targeted Node MCP and contract checks passed. HTTP conformance
passed again with 42 Node and 42 Rust cases, using that exact v1.12.1 binary. Cargo formatting,
JavaScript syntax, diff checks, CI YAML parsing, and all ten browser test budgets also passed.
The version update changes no viewer, gallery, or folder implementation.

All browser mutations used disposable local identities, organizations, databases, artifact files,
and speech workers. Browser evidence contains synthetic content. The protected production backup
and private deployment records are not part of this repository.

## Completed application checks

| Check | Result |
| --- | --- |
| Node unit and integration checks | 522 passed |
| Rust checks across all targets | 920 passed |
| Cross-runtime HTTP conformance | 42 Node and 42 Rust cases passed |
| Full Chromium browser suite | 218 passed, two expected Node discussion skips, zero failures |
| Audio reader regressions | 14 executions passed, zero skips |
| Saved and draft comment positions | 60 comparisons passed within two CSS pixels |
| Firefox and WebKit desktop/phone matrix | Eight cases passed with zero console, page, HTTP, or transport errors |
| Formatting, JavaScript syntax, and diff checks | Passed |

The [Chromium log](evidence/inline-release-20261005/chromium-tests.txt) includes folder creation,
membership changes, folder tools through MCP, ribbon reordering and reload persistence,
keyboard and pointer controls, all library views, and viewer startup. The new performance cases
check search order, omitted inactive folder cards, deferred discussion loading, voice capability
states, inline asset delivery, and an artifact frame that loads before the main viewer script.

The [Firefox and WebKit evidence](evidence/inline-release-20261005/cross-browser.json) covers both
runtimes at 1440 by 900 and 390 by 844. It includes real streamed audio, pause, reload and resume,
search and sort, four library views, menu bounds, lazy Details, early state readiness, and saved
comment positions. These are Playwright browser engines, not tests on physical phones or Safari.
The gallery and viewer made zero application requests under `/assets/`.

The [position evidence](evidence/inline-release-20261005/anchor-geometry.json) compares the actual
picked paragraph with its saved and draft overlays. It uses three viewport sizes and five scroll
positions on each runtime. The draft comment form follows the live selection.

Representative screenshots were inspected, including the
[phone gallery](evidence/inline-release-20261005/rust-webkit-390-gallery.png),
[phone viewer](evidence/inline-release-20261005/rust-webkit-390-viewer.png), and
[desktop gallery](evidence/inline-release-20261005/rust-firefox-1440-gallery.png).
The phone controls and menus fit within the viewport. Preview workers were not enabled for these
small synthetic browser fixtures, so some screenshots show the existing preview fallback.

## False failures from test request limits

The first full runs used speech-reader fixture limits that were too low for the complete browser
suite. Administrator and verified-viewer requests first reached those limits. After those limits
were corrected, a run passed 216 checks and failed two Rust folder interactions.

The folder failures both showed `Collection request failed`. Endpoint capture reproduced the
ordinary read bucket returning `429` with
`{"error":"too many requests","code":"rate_limited"}`. The frozen production binary reproduced
the same response. The original bucket admitted about 1,000 requests in its window. The corrected
test launcher admitted all 1,100 probe requests.

With sufficient test budgets, the 25 affected folder checks passed on the production binary and
in three fresh candidate runs. The complete candidate suite then passed 218 checks with the same
two expected skips. There was no application change to hide these failures. See the
[diagnosis receipt](evidence/inline-release-20261005/request-limit-diagnosis.json).

Browser CI now sets all ten ingress request budgets to 10,000 for its isolated test server.
The production request limits and security checks are unchanged.

## Comparison with current production

The comparison uses the verified v1.12.0 production binary and the frozen four-change test binary.
Each has 1,000 synthetic artifacts and 12 folders. Chromium renders the All artifacts view at
1440 by 900 with four-times CPU throttling. There are two fresh browser contexts and a second
navigation in each context, for four gallery samples per binary. Each sample runs five searches.
There is no network simulation, content-visibility experiment, or viewer benchmark in this check.
Other browser tests and compilation were stopped during measurement.

| Median measurement | Production v1.12.0 | Four-change candidate |
| --- | --- | --- |
| Search event handler, all 20 searches | 5,092 ms | 20.9 ms |
| Search to two browser frames, all 20 searches | 5,416 ms | 1,105 ms |
| Gallery ready, first navigation in each context | 19,847 ms | 7,322 ms |
| Gallery ready, second navigation in each context | 17,669 ms | 6,528 ms |

The handler time measures JavaScript work during the search event. The two-frame measurement
includes the browser work needed to display the changed result. It is the more useful estimate
of visible response in this test. Even the candidate takes about one second with 1,000 cards on
the throttled browser. Its page still has about 84,000 elements and 5.4 MB of uncompressed HTML.

All samples preserved 1,000 canonical cards and restored all cards after clearing search.
No console, page, HTTP, or unexpected transport error occurred. Candidate counters recorded one
sort run across seven filter applications and no sort reordering for the unchanged sort choice.
The older instrumentation misses card moves through a document fragment, so its zero move counts
cannot be used to compare DOM movement. DOM means the browser's tree of page elements.

See the [production samples](evidence/inline-release-20261005/baseline-v112.json) and
[candidate samples](evidence/inline-release-20261005/candidate-v112.json). This small local sample
shows direction and scale under this workload. It does not predict production network latency,
physical phone performance, or the performance of every folder view.

## Release decision

Local application QA found no remaining release blocker in the four-change candidate. The public
release workflow still needs to pass on the final source revision and produce a verified binary,
image, manifest, checksums, and build attestations. The local test binary is not a deployment asset.

A protected encrypted backup passed decryption, SQLite integrity, schema-38, artifact/revision
coherence, and file-presence checks. Selected artifact body and revision hashes matched. These
checks do not claim a complete restored application boot or an offsite upload. The current
v1.12.0 binary is the rollback target, and production must be checked again before installation.
The backup must be refreshed if it exceeds the private deployment procedure's freshness limit.

The same-schema rollback check also passed. The encrypted backup was restored to protected
temporary memory storage. The exact local v1.12.1 binary started first, then the verified v1.12.0
binary started against the same copy. Both returned successful health, gallery, viewer, raw
artifact, and collection responses at schema 38. Artifact, revision, key registry, binding,
source, folder, membership, preference, and artifact-file fingerprints stayed unchanged.
Viewer reads can update view counters, which are outside these body and revision comparisons.

Both processes used a private network and process namespace with only loopback connectivity.
The copied signed audit history required its original verification key. The authorized secrets
manager supplied that one key to the private runner, with no key values in commands, logs, or
files. A synthetic webhook encryption key was sufficient for these reads. This check does not
verify encrypted integration connections or external workers. The restored copy did not exercise
service-principal folder ownership; separate synthetic MCP and conformance checks cover that
behavior. All test processes, plaintext restore files, logs, and the temporary mount were removed.
Production was not changed.

The earlier [five-change experiment](web-interface-20261005.md) and
[asset rollback QA](web-interface-qa-20261005.md) describe a different candidate based on v1.11.2.
Their shared asset caching and content-visibility measurements do not describe this release.
The gallery still renders every artifact. Very large libraries still need a separate bounded
rendering or pagination design. No production Cloudflare latency or compression improvement is
claimed by these local checks.
