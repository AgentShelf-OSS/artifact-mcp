// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Runs the JavaScript unit tests in `browser-tests/js/*.test.js` inside real Chrome.
//!
//! Each test file gets its own browser and page, like `node --test` gives each file its own
//! process. The page lives on a virtual origin, [`ORIGIN`], served from the repository by
//! [`Page::route`]: nothing touches the network, and the browser runs with the renderer's network
//! lockdown. The file is imported as an ES module; its `test(name, fn)` calls register cases in
//! `js/harness.js`; then Rust runs the cases one at a time with `harness.run(index)`.
//!
//! A case fails when it throws or rejects, and also when anything it causes is logged as a
//! failure while it runs: `console.error`, a failed `console.assert`, an uncaught exception, an
//! unhandled rejection, a dialog or a crash ([`LogEntry::is_failure`]). Failures logged while the
//! module loads, or after the last case, fail the file.
//!
//! Some checks need DevTools rather than page JavaScript and run as Rust [`Suite`]s with the same
//! reporting: today `browser-assets-parse`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{Browser, BrowserOptions, Error, LogEntry, Page, Response, StaticFiles};

/// The virtual origin the test pages live on. `https` makes it a secure context; `.test` never
/// resolves.
pub const ORIGIN: &str = "https://harness.test";

/// The empty page each suite starts from.
const BLANK_PATH: &str = "/__harness__/blank.html";
const BLANK_HTML: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>browser-tests</title></head><body></body></html>";

/// Values computed in Rust for the JavaScript side: `await fixture(name)` in `js/harness.js`.
const FIXTURE_PREFIX: &str = "/__fixtures__/";

/// The in-page runner, as imported by every test file.
const HARNESS_URL: &str = "/browser-tests/js/harness.js";

/// The repository-relative directory of the JavaScript test files.
pub const JS_DIR: &str = "browser-tests/js";

const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
const CASE_TIMEOUT: Duration = Duration::from_secs(60);

/// The Rust-side suite that parse-checks every browser asset.
pub const ASSETS_PARSE: &str = "browser-assets-parse";
const ASSETS_PARSE_CASE: &str = "every browser asset parses";

/// One test file, or one Rust-side check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suite {
    /// The file name without `.test.js`, or the Rust suite's name.
    pub name: String,
    kind: SuiteKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SuiteKind {
    /// A repository-relative module path.
    Js(String),
    AssetsParse,
}

/// Every suite: the `*.test.js` files in [`JS_DIR`], then the Rust-side checks, sorted by name.
pub fn discover(repo_root: &Path) -> std::io::Result<Vec<Suite>> {
    let mut suites = Vec::new();
    for entry in std::fs::read_dir(repo_root.join(JS_DIR))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".test.js") {
            suites.push(Suite {
                name: stem.to_owned(),
                kind: SuiteKind::Js(format!("{JS_DIR}/{name}")),
            });
        }
    }
    suites.push(Suite {
        name: ASSETS_PARSE.to_owned(),
        kind: SuiteKind::AssetsParse,
    });
    suites.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(suites)
}

/// The outcome of one case.
#[derive(Debug, Clone)]
pub struct CaseResult {
    pub name: String,
    /// `None` when the case passed.
    pub failure: Option<String>,
    /// Everything the page logged while the case ran.
    pub log: Vec<LogEntry>,
    pub duration: Duration,
}

/// The outcome of one suite.
#[derive(Debug, Clone)]
pub struct SuiteReport {
    pub suite: String,
    pub cases: Vec<CaseResult>,
    /// Cases the filter skipped.
    pub filtered: usize,
    /// Failures that belong to no case: the module did not load, or something failed after the
    /// last case.
    pub errors: Vec<String>,
    pub duration: Duration,
}

impl SuiteReport {
    pub fn passed(&self) -> bool {
        self.errors.is_empty() && self.cases.iter().all(|case| case.failure.is_none())
    }
}

/// The full name of a case, as printed and as matched by filters.
pub fn full_name(suite: &str, case: &str) -> String {
    format!("{suite} :: {case}")
}

/// Run `suite` in a fresh browser. `selected` gets each case's full name; cases it rejects are
/// skipped (counted in [`SuiteReport::filtered`]).
pub async fn run(
    chrome: &Path,
    repo_root: &Path,
    suite: &Suite,
    selected: &dyn Fn(&str) -> bool,
) -> SuiteReport {
    let started = Instant::now();
    let mut report = SuiteReport {
        suite: suite.name.clone(),
        cases: Vec::new(),
        filtered: 0,
        errors: Vec::new(),
        duration: Duration::ZERO,
    };
    match Browser::launch(&BrowserOptions::new(chrome)).await {
        Ok(browser) => {
            if let Err(error) = run_in(&browser, repo_root, suite, selected, &mut report).await {
                report.errors.push(error.to_string());
            }
            browser.close().await;
        }
        Err(error) => report.errors.push(error.to_string()),
    }
    report.duration = started.elapsed();
    report
}

/// The case names of `suite`, without running any.
pub async fn list(chrome: &Path, repo_root: &Path, suite: &Suite) -> Result<Vec<String>, Error> {
    match &suite.kind {
        SuiteKind::AssetsParse => Ok(vec![ASSETS_PARSE_CASE.to_owned()]),
        SuiteKind::Js(module) => {
            let browser = Browser::launch(&BrowserOptions::new(chrome)).await?;
            let names = async {
                let page = open_repo_page(&browser, repo_root).await?;
                load_cases(&page, module).await
            }
            .await;
            browser.close().await;
            names
        }
    }
}

async fn run_in(
    browser: &Browser,
    repo_root: &Path,
    suite: &Suite,
    selected: &dyn Fn(&str) -> bool,
    report: &mut SuiteReport,
) -> Result<(), Error> {
    let page = open_repo_page(browser, repo_root).await?;
    let mut cursor = page.log_mark();
    match &suite.kind {
        SuiteKind::Js(module) => {
            let names = load_cases(&page, module).await;
            report
                .errors
                .extend(failures(&page.log_after(&mut cursor), "while loading"));
            for (index, name) in names?.into_iter().enumerate() {
                if !selected(&full_name(&suite.name, &name)) {
                    report.filtered += 1;
                    continue;
                }
                let started = Instant::now();
                let outcome = page
                    .evaluate_with_timeout(
                        &format!(
                            "import({}).then((harness) => harness.run({index}))",
                            json!(HARNESS_URL)
                        ),
                        CASE_TIMEOUT,
                    )
                    .await;
                let log = page.log_after(&mut cursor);
                let mut problems = Vec::new();
                match outcome {
                    Ok(result) if result.get("ok") == Some(&Value::Bool(true)) => {}
                    Ok(result) => problems.push(
                        result
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("the case failed without an error")
                            .to_owned(),
                    ),
                    Err(error) => problems.push(error.to_string()),
                }
                problems.extend(failures(&log, "while the case ran"));
                report.cases.push(CaseResult {
                    name,
                    failure: (!problems.is_empty()).then(|| problems.join("\n")),
                    log,
                    duration: started.elapsed(),
                });
            }
            // Give timers and callbacks the cases left behind one more turn to fail.
            page.evaluate("new Promise((resolve) => setTimeout(resolve, 20))")
                .await?;
            report.errors.extend(failures(
                &page.log_after(&mut cursor),
                "after the last case",
            ));
        }
        SuiteKind::AssetsParse => {
            if selected(&full_name(&suite.name, ASSETS_PARSE_CASE)) {
                let started = Instant::now();
                let mut problems = match check_assets_parse(&page, repo_root).await {
                    Ok(problems) => problems,
                    Err(error) => vec![error.to_string()],
                };
                let log = page.log_after(&mut cursor);
                problems.extend(failures(&log, "while parsing"));
                report.cases.push(CaseResult {
                    name: ASSETS_PARSE_CASE.to_owned(),
                    failure: (!problems.is_empty()).then(|| problems.join("\n")),
                    log,
                    duration: started.elapsed(),
                });
            } else {
                report.filtered += 1;
            }
        }
    }
    Ok(())
}

fn failures(log: &[LogEntry], when: &str) -> Vec<String> {
    log.iter()
        .filter(|entry| entry.is_failure())
        .map(|entry| format!("{when}: {entry}"))
        .collect()
}

/// A page on [`ORIGIN`] that serves the repository read-only, plus the blank page and the
/// fixtures, opened at the blank page. Test files and `js/harness.js` are importable from it.
pub async fn open_repo_page(browser: &Browser, repo_root: &Path) -> Result<Page, Error> {
    let page = browser.new_page().await?;
    let files = StaticFiles::new(repo_root);
    let root = repo_root.to_path_buf();
    page.route(&format!("{ORIGIN}/"), move |request| {
        if request.path == BLANK_PATH {
            return Response::ok("text/html; charset=utf-8", BLANK_HTML);
        }
        if let Some(name) = request.path.strip_prefix(FIXTURE_PREFIX) {
            return match fixture(&root, name) {
                Ok(text) => Response::ok("text/plain; charset=utf-8", text),
                Err(error) => Response::error(500, &error),
            };
        }
        files.respond(request)
    })
    .await?;
    page.goto(&format!("{ORIGIN}{BLANK_PATH}")).await?;
    Ok(page)
}

/// Import the test module and return the names it registered.
async fn load_cases(page: &Page, module: &str) -> Result<Vec<String>, Error> {
    let names = page
        .evaluate_with_timeout(
            &format!(
                "import({}).then(() => import({})).then((harness) => harness.cases())",
                json!(format!("/{module}")),
                json!(HARNESS_URL)
            ),
            LOAD_TIMEOUT,
        )
        .await
        .map_err(|error| Error::Evaluation(format!("{module} did not load: {error}")))?;
    let names: Vec<String> = names
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        return Err(Error::Evaluation(format!("{module} registered no tests")));
    }
    Ok(names)
}

/// A value computed in Rust for `await fixture(name)`.
fn fixture(repo_root: &Path, name: &str) -> Result<String, String> {
    match name {
        // The anchor bridge the server injects into artifact HTML: the exact bytes of the raw
        // string constant in src/http/artifact_response.rs.
        "ANCHOR_BRIDGE" => {
            let path = repo_root.join("src/http/artifact_response.rs");
            let source = std::fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            rust_raw_string(&source, "ANCHOR_BRIDGE")
        }
        other => Err(format!(
            "unknown fixture {other:?}; add it to browser-tests/src/suite.rs"
        )),
    }
}

/// The body of `pub const NAME: &str = r###"…"###;` in Rust source.
pub fn rust_raw_string(source: &str, constant: &str) -> Result<String, String> {
    let marker = format!("pub const {constant}: &str = r###\"");
    let start = source
        .find(&marker)
        .ok_or_else(|| format!("could not find {constant}"))?
        + marker.len();
    let length = source[start..]
        .find("\"###;")
        .ok_or_else(|| format!("could not find the end of {constant}"))?;
    Ok(source[start..start + length].to_owned())
}

/// Parse every `assets/*.js` file in Chrome, without running it: as a classic script (how the
/// pages load them) and as an ES module (how `node --check` checked them before, under the
/// root package's `"type": "module"`, and how the pitch worklet loads).
///
/// The viewer shell is one closure split across `assets/shell/` parts, so it is parsed as the
/// server ships it: the parts joined in `assets/shell/parts.txt` order.
///
/// `assets/shell.js` once shipped truncated: the closing `})();` of its IIFE was lost, so the
/// browser could not parse the block and the reaction, share and comment controls were inert.
/// Every server-side test still passed, because nothing executed page JavaScript. A brace-balance
/// check is not a substitute (regex literals such as `/[a-z{]/` contain unbalanced braces), so
/// this uses the real parser.
async fn check_assets_parse(page: &Page, repo_root: &Path) -> Result<Vec<String>, Error> {
    let dir = repo_root.join("assets");
    let mut assets: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|error| Error::Protocol(format!("{}: {error}", dir.display())))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file() && path.extension().is_some_and(|extension| extension == "js")
        })
        .collect();
    assets.sort();
    if assets.is_empty() {
        return Ok(vec!["no browser assets were checked".to_owned()]);
    }
    let mut broken = Vec::new();
    for asset in &assets {
        let name = asset
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let url = format!("{ORIGIN}/assets/{name}");
        let source = match std::fs::read_to_string(asset) {
            Ok(source) => source,
            Err(error) => {
                broken.push(format!("{name}: {error}"));
                continue;
            }
        };
        if let Some(error) = page.compile_script(&source, &url).await? {
            broken.push(format!("{name} (classic script): {error}"));
        }
        if let Some(error) = page.compile_module(&source, &url).await? {
            broken.push(format!("{name} (ES module): {error}"));
        }
    }
    let name = "shell/parts.txt (joined)";
    match joined_shell(&dir) {
        Ok(source) => {
            let url = format!("{ORIGIN}/assets/shell.js");
            if let Some(error) = page.compile_script(&source, &url).await? {
                broken.push(format!("{name} (classic script): {error}"));
            }
            if let Some(error) = page.compile_module(&source, &url).await? {
                broken.push(format!("{name} (ES module): {error}"));
            }
        }
        Err(error) => broken.push(format!("{name}: {error}")),
    }
    if broken.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![format!(
            "browser assets the browser cannot parse:\n  {}",
            broken.join("\n  ")
        )])
    }
}

/// The viewer shell script: the `assets/shell/` parts joined in `assets/shell/parts.txt` order.
fn joined_shell(assets: &Path) -> std::io::Result<String> {
    let shell = assets.join("shell");
    let listing = std::fs::read_to_string(shell.join("parts.txt"))?;
    let mut source = String::new();
    for name in listing.lines().filter(|name| !name.is_empty()) {
        source.push_str(&std::fs::read_to_string(shell.join(name))?);
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_joined_shell_is_one_closed_closure() {
        let shell = joined_shell(&crate::repo_root().join("assets")).unwrap();
        assert!(shell.trim_start().starts_with("(function(){"));
        assert!(shell.ends_with("})();\n"));
    }

    #[test]
    fn raw_strings_are_extracted_exactly() {
        let source = "x\npub const A: &str = r###\"<script>\"quoted\"</script>\"###;\n";
        assert_eq!(
            rust_raw_string(source, "A").unwrap(),
            "<script>\"quoted\"</script>"
        );
        assert!(rust_raw_string(source, "B").is_err());
        assert!(rust_raw_string("pub const A: &str = r###\"open", "A").is_err());
    }

    #[test]
    fn the_anchor_bridge_fixture_comes_from_the_server_source() {
        let bridge = fixture(&crate::repo_root(), "ANCHOR_BRIDGE").unwrap();
        assert!(
            bridge.starts_with("<script"),
            "{}",
            &bridge[..40.min(bridge.len())]
        );
        assert!(bridge.trim_end().ends_with("</script>"));
        assert!(fixture(&crate::repo_root(), "NOPE").is_err());
    }

    #[test]
    fn discovery_finds_the_js_files_and_the_rust_checks() {
        let suites = discover(&crate::repo_root()).unwrap();
        assert!(suites.iter().any(|suite| suite.name == ASSETS_PARSE));
        assert!(
            suites.iter().any(
                |suite| matches!(&suite.kind, SuiteKind::Js(path) if path.ends_with(".test.js"))
            )
        );
        let mut names: Vec<&str> = suites.iter().map(|suite| suite.name.as_str()).collect();
        names.dedup();
        assert_eq!(names.len(), suites.len(), "suite names are unique");
    }
}
