// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! End-to-end tests of artifact-mcp in a real browser: the fixtures `playwright/fixtures.mjs`,
//! `global-setup.mjs` and `global-teardown.mjs` provided, and the runner behind
//! `cargo test --test e2e`.
//!
//! * **Global setup** ([`Fixture::setup`]) starts the server ([`crate::server`]), creates the
//!   throwaway organization `pwtest-<run id>` and launches one browser ([`Network::LoopbackOnly`]:
//!   pages reach the server and nothing else).
//! * **Each test** gets a [`Ctx`]: pages with an identity ([`Ctx::page`] is Playwright's `page`
//!   fixture, signed in as the admin; [`Ctx::page_as`] is `browser.newContext({ extraHTTPHeaders
//!   })`), API clients ([`Ctx::request`], [`Ctx::api`]) and the publishing helpers. Every page has
//!   its own browser context; the runner disposes of all of them after the test.
//! * **Global teardown** ([`Fixture::teardown`]) deletes the artifacts of the run's organizations
//!   and the organizations, and fails the run if it cannot.
//!
//! A test is an `async fn(Ctx) -> Result<(), Error>`, registered with [`case`](crate::e2e_case).
//! It fails when it returns an error, panics (`assert!`), or runs longer than 30 s (Playwright's
//! test timeout; `E2E_TEST_TIMEOUT_MS` changes it).

use std::any::Any;
use std::collections::HashSet;
use std::future::Future;
use std::ops::Deref;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::connection::lock;
use crate::http::{Body, Client, HttpResponse};
use crate::server::{ADMIN_EMAIL, Server, ServerOptions};
use crate::{Browser, BrowserOptions, Error, Network, Page};

/// The identity header Cloudflare Access sets, which the test server trusts.
pub const IDENTITY_HEADER: &str = "Cf-Access-Authenticated-User-Email";

/// Who a page or API client is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// The administrator (Playwright's default `extraHTTPHeaders`).
    Admin,
    /// A signed-in user.
    Email(String),
    /// No identity header at all (`extraHTTPHeaders: {}`).
    Anonymous,
    /// Exactly these headers on every request.
    Headers(Vec<(String, String)>),
}

impl Identity {
    pub fn email(email: impl Into<String>) -> Self {
        Self::Email(email.into())
    }

    fn headers(&self, admin: &str) -> Vec<(String, String)> {
        match self {
            Self::Admin => vec![(IDENTITY_HEADER.into(), admin.to_owned())],
            Self::Email(email) => vec![(IDENTITY_HEADER.into(), email.clone())],
            Self::Anonymous => Vec::new(),
            Self::Headers(headers) => headers.clone(),
        }
    }
}

fn borrowed(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// Milliseconds since the epoch (JavaScript's `Date.now()`).
pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

/// Eight lowercase hex digits, unique within the process (for client ids and emails, where the
/// JavaScript suite used `randomUUID().slice(0, 8)`).
pub fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mixed = (nanos as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(
            COUNTER
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_mul(0xBF58_476D_1CE4_E5B9),
        )
        ^ u64::from(std::process::id());
    format!("{:08x}", mixed >> 32)
}

/// `encodeURIComponent`.
pub fn encode_component(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// What every test shares: the server, the run's organization and the browser.
pub struct Fixture {
    server: Server,
    chrome: PathBuf,
    browser: Mutex<Option<Arc<Browser>>>,
    /// `E2E_RUN_ID`, or the last six digits of the start time.
    pub run_id: String,
    /// `pwtest-<run id>`.
    pub org: String,
    pub admin: String,
}

impl Fixture {
    /// Global setup: start the server, create the run's organization, launch the browser.
    pub async fn setup(chrome: PathBuf, binary: PathBuf) -> Result<Self, Error> {
        let mut options = ServerOptions::new(binary);
        // The one managed credential variable the Connections page may offer.
        options
            .env
            .push((CREDENTIAL_VARIABLE.into(), "Bearer e2e-credential".into()));
        let server = Server::start(&options).await?;
        let run_id = std::env::var("E2E_RUN_ID")
            .ok()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| {
                let now = now_millis().to_string();
                now[now.len().saturating_sub(6)..].to_owned()
            });
        let fixture = Self {
            server,
            chrome,
            browser: Mutex::new(None),
            org: format!("pwtest-{run_id}"),
            run_id,
            admin: ADMIN_EMAIL.to_owned(),
        };
        let created = fixture
            .api(
                "POST",
                "/settings/orgs",
                Some(json!({"name": fixture.org, "label": "Playwright Test"})),
            )
            .await?;
        if !created.ok() && created.status != 400 {
            return Err(Error::Assertion(format!(
                "could not create {} on {}: {} {}",
                fixture.org,
                fixture.base_url(),
                created.status,
                created.text()
            )));
        }
        fixture.ensure_browser().await?;
        println!("[setup] throwaway org: {}", fixture.org);
        Ok(fixture)
    }

    /// The server's base URL (Playwright's `baseURL`).
    pub fn base_url(&self) -> &str {
        self.server.base_url()
    }

    pub fn server(&self) -> &Server {
        &self.server
    }

    /// The browser, relaunched if the previous one died.
    pub async fn ensure_browser(&self) -> Result<Arc<Browser>, Error> {
        let current = lock(&self.browser).clone();
        if let Some(browser) = current {
            if browser.version().await.is_ok() {
                return Ok(browser);
            }
            eprintln!("[e2e] the browser stopped answering; launching a new one");
        }
        let browser = Arc::new(
            Browser::launch(&BrowserOptions::new(&self.chrome).network(Network::LoopbackOnly))
                .await?,
        );
        *lock(&self.browser) = Some(Arc::clone(&browser));
        Ok(browser)
    }

    fn browser(&self) -> Result<Arc<Browser>, Error> {
        lock(&self.browser)
            .clone()
            .ok_or_else(|| Error::Closed("the browser is not running".into()))
    }

    /// Close every page of the last test (their browser contexts).
    pub async fn dispose_pages(&self) {
        if let Ok(browser) = self.browser() {
            let _ = browser.dispose_contexts().await;
        }
    }

    /// The headers `fixtures.mjs` called `adminHeaders`: the admin identity plus the portal's
    /// request-authenticity headers.
    pub fn admin_headers(&self) -> Vec<(String, String)> {
        vec![
            (IDENTITY_HEADER.into(), self.admin.clone()),
            ("X-Artifact-Mutation".into(), "1".into()),
            ("Sec-Fetch-Site".into(), "same-origin".into()),
        ]
    }

    /// Playwright's `request` fixture: API calls as the admin, without the mutation headers.
    pub fn request(&self) -> Client {
        self.request_as(&Identity::Admin)
    }

    /// API calls with `identity` (Playwright's `context.request` of a context created with those
    /// `extraHTTPHeaders`).
    pub fn request_as(&self, identity: &Identity) -> Client {
        Client::new(self.base_url(), &borrowed(&identity.headers(&self.admin)))
    }

    /// `fixtures.mjs` `api()`: `method` on `path` with the admin headers and an optional JSON
    /// body.
    pub async fn api(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<HttpResponse, Error> {
        let headers = self.admin_headers();
        self.request()
            .send(method, path, body.map(Body::Json), &borrowed(&headers))
            .await
    }

    /// The `publisherKey` fixture: a new publisher key for the run's organization.
    pub async fn publisher_key(&self) -> Result<String, Error> {
        let client_id = format!("pwkey-{}-{}", self.run_id, unique_suffix());
        let response = self
            .api(
                "POST",
                "/settings/keys",
                Some(json!({"clientId": client_id, "org": self.org, "label": "playwright"})),
            )
            .await?;
        let body = response.json();
        key_secret(&body).ok_or_else(|| {
            let text = body.to_string();
            Error::Assertion(format!(
                "publisher key not minted: {} {}",
                response.status,
                &text[..text.len().min(200)]
            ))
        })
    }

    /// Call the MCP tool `name` with a publisher key; returns the JSON-RPC reply.
    pub async fn mcp_call(
        &self,
        key: &str,
        name: &str,
        arguments: Value,
    ) -> Result<HttpResponse, Error> {
        self.request()
            .send(
                "POST",
                "/mcp",
                Some(Body::Json(json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": name, "arguments": arguments},
                }))),
                &[("authorization", &format!("Bearer {key}"))],
            )
            .await
    }

    /// `fixtures.mjs` `publish()`: publish an artifact with `key`; returns the tool result (with
    /// `id`).
    pub async fn publish(
        &self,
        key: &str,
        title: &str,
        html: &str,
        category: Option<&str>,
    ) -> Result<Value, Error> {
        let mut arguments =
            json!({"title": title, "description": "playwright fixture", "html": html});
        if let Some(category) = category {
            arguments["category"] = json!(category);
        }
        let response = self.mcp_call(key, "publish_artifact", arguments).await?;
        let reply = response.json();
        let text = reply
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        serde_json::from_str(text).map_err(|error| {
            Error::Protocol(format!("publish_artifact returned {text:?}: {error}"))
        })
    }

    /// A page signed in as `identity`, in its own browser context, with the base URL set and
    /// Playwright's defaults (evaluation with a user gesture, browser messages in the log).
    pub async fn page_as(&self, identity: Identity) -> Result<Page, Error> {
        let browser = self.browser()?;
        let mut page = browser.new_page().await?;
        page.set_base_url(self.base_url());
        page.set_evaluate_user_gesture(true);
        let headers = identity.headers(&self.admin);
        page.set_extra_http_headers(&borrowed(&headers)).await?;
        page.enable_browser_log().await?;
        Ok(page)
    }

    /// Playwright's `page` fixture: a page signed in as the admin.
    pub async fn page(&self) -> Result<Page, Error> {
        self.page_as(Identity::Admin).await
    }

    /// Global teardown (`global-teardown.mjs`): delete the artifacts of the run's organizations,
    /// then the organizations.
    pub async fn teardown(&self) -> Result<String, Error> {
        let org = &self.org;
        let listed = self.api("GET", "/", None).await?;
        if !listed.ok() {
            return Err(Error::Assertion(format!(
                "QA fixture cleanup failed: could not list QA artifacts: {}",
                listed.status
            )));
        }
        let html = listed.text();
        let existing = org_filter_options(&html);
        let owned: HashSet<String> = [
            org.clone(),
            format!("{org}-other"),
            format!("{org}-reset-other"),
            org.replacen("pwtest-", "pwcat-", 1),
        ]
        .into_iter()
        .collect();
        let ids = owned_card_ids(&html, &owned);
        for id in &ids {
            let removed = self.api("DELETE", &format!("/{id}"), None).await?;
            if !removed.ok() {
                return Err(Error::Assertion(format!(
                    "QA fixture cleanup failed: could not delete QA artifact: {}",
                    removed.status
                )));
            }
        }
        for name in &owned {
            if !existing.contains(name) {
                continue;
            }
            let removed = self
                .api(
                    "DELETE",
                    &format!("/settings/orgs/{}", encode_component(name)),
                    None,
                )
                .await?;
            if !removed.ok() && removed.status != 404 {
                return Err(Error::Assertion(format!(
                    "QA fixture cleanup failed: could not delete {name}: {}",
                    removed.status
                )));
            }
        }
        Ok(format!(
            "[teardown] removed {org} and companion organizations ({} artifact(s)) on {}",
            ids.len(),
            self.base_url()
        ))
    }
}

fn key_secret(body: &Value) -> Option<String> {
    ["secret", "key"]
        .iter()
        .find_map(|name| body.get(*name).and_then(Value::as_str))
        .filter(|secret| !secret.is_empty())
        .map(str::to_owned)
}

/// The `value`s of the options of `<select id="org-filter" ...>`.
fn org_filter_options(html: &str) -> HashSet<String> {
    let Some(start) = html.find("<select id=\"org-filter\"") else {
        return HashSet::new();
    };
    let rest = &html[start..];
    let Some(open_end) = rest.find('>') else {
        return HashSet::new();
    };
    let body = &rest[open_end + 1..];
    let body = &body[..body.find("</select>").unwrap_or(body.len())];
    body.match_indices("<option value=\"")
        .filter_map(|(at, marker)| {
            let value = &body[at + marker.len()..];
            value.find('"').map(|end| value[..end].to_owned())
        })
        .filter(|value| !value.is_empty())
        .collect()
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// An attribute value in a tag, where the name starts at a word boundary.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("{name}=\"");
    let mut from = 0;
    while let Some(found) = tag[from..].find(&marker) {
        let at = from + found;
        if at == 0 || !is_word(tag.as_bytes()[at - 1]) {
            let value = &tag[at + marker.len()..];
            let end = value.find('"')?;
            return (end > 0).then(|| &value[..end]);
        }
        from = at + marker.len();
    }
    None
}

/// The `data-id`s of the canonical gallery cards (`<article class="card ...">` before
/// `</main>`) whose `data-org` is one of `orgs`.
fn owned_card_ids(html: &str, orgs: &HashSet<String>) -> Vec<String> {
    let main = html.split("</main>").next().unwrap_or("");
    let bytes = main.as_bytes();
    let mut ids = Vec::new();
    for (at, _) in main.match_indices("<article") {
        if bytes.get(at + 8).is_some_and(|byte| is_word(*byte)) {
            continue;
        }
        let Some(end) = main[at..].find('>') else {
            break;
        };
        let tag = &main[at..at + end + 1];
        let is_card = tag.match_indices("class=\"card").any(|(position, marker)| {
            !tag.as_bytes()
                .get(position + marker.len())
                .is_some_and(|byte| is_word(*byte))
        });
        if !is_card {
            continue;
        }
        if attribute(tag, "data-org").is_some_and(|org| orgs.contains(org))
            && let Some(id) = attribute(tag, "data-id")
        {
            ids.push(id.to_owned());
        }
    }
    ids
}

/// The `ARTIFACT_DATA_CREDENTIAL_*` variable set on the server under test.
pub const CREDENTIAL_VARIABLE: &str = "ARTIFACT_DATA_CREDENTIAL_E2E";

/// One test's view of the run: the shared [`Fixture`] (through `Deref`).
#[derive(Clone)]
pub struct Ctx {
    fixture: Arc<Fixture>,
}

impl Ctx {
    pub fn new(fixture: Arc<Fixture>) -> Self {
        Self { fixture }
    }
}

impl Deref for Ctx {
    type Target = Fixture;

    fn deref(&self) -> &Fixture {
        &self.fixture
    }
}

/// The future a test returns.
pub type TestFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

/// One registered test.
#[derive(Clone, Copy)]
pub struct Case {
    /// The spec (module) name, for example `01-gallery`.
    pub spec: &'static str,
    /// The test title, kept from the Playwright suite.
    pub title: &'static str,
    pub run: fn(Ctx) -> TestFuture,
}

impl Case {
    /// `spec :: title`, the name filters match.
    pub fn full_name(&self) -> String {
        format!("{} :: {}", self.spec, self.title)
    }
}

/// Register `async fn(Ctx) -> Result<(), Error>` as a test titled `$title` of spec `$spec`.
#[macro_export]
macro_rules! e2e_case {
    ($spec:expr, $title:expr, $function:path) => {
        $crate::e2e::Case {
            spec: $spec,
            title: $title,
            run: |ctx| Box::pin($function(ctx)),
        }
    };
}

struct Args {
    filters: Vec<String>,
    exact: bool,
    list: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        filters: Vec::new(),
        exact: false,
        list: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--exact" => args.exact = true,
            "--list" => args.list = true,
            "--format" | "--color" | "--skip" | "-Z" | "--logfile" | "--report-time"
            | "--test-threads" => {
                let _ = iter.next();
            }
            other if other.starts_with('-') => {}
            filter => args.filters.push(filter.to_owned()),
        }
    }
    args
}

impl Args {
    fn selects(&self, name: &str) -> bool {
        self.filters.is_empty()
            || self.filters.iter().any(|filter| {
                if self.exact {
                    name == filter
                } else {
                    name.contains(filter.as_str())
                }
            })
    }
}

fn panic_text(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panicked".to_owned())
}

/// The `main` of `tests/e2e`: run the selected cases one at a time (Playwright's `workers: 1`)
/// against one server and one browser, libtest style. Failure screenshots go to `failure_dir`.
///
/// Without `CHROME_HEADLESS_SHELL` or `ARTIFACT_MCP_BIN` the run is skipped, unless `CI` is set,
/// where it fails.
pub fn main(cases: &[Case], failure_dir: PathBuf) -> ExitCode {
    let args = parse_args();
    let selected: Vec<Case> = cases
        .iter()
        .copied()
        .filter(|case| args.selects(&case.full_name()))
        .collect();
    if args.list {
        for case in &selected {
            println!("{}: test", case.full_name());
        }
        println!("\n{} tests, 0 benchmarks", selected.len());
        return ExitCode::SUCCESS;
    }
    let chrome = match std::env::var_os("CHROME_HEADLESS_SHELL") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ if std::env::var_os("CI").is_some() => {
            eprintln!(
                "CHROME_HEADLESS_SHELL must be set in CI; end-to-end tests may not be skipped there"
            );
            return ExitCode::FAILURE;
        }
        _ => {
            println!(
                "\nrunning 0 tests: CHROME_HEADLESS_SHELL is not set, end-to-end tests skipped\n"
            );
            return ExitCode::SUCCESS;
        }
    };
    let binary = match crate::server::binary_from_env() {
        Ok(Some(binary)) => binary,
        Ok(None) => {
            println!("\nrunning 0 tests: ARTIFACT_MCP_BIN is not set, end-to-end tests skipped\n");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let filtered = cases.len() - selected.len();
    crate::expect::set_failure_dir(failure_dir);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(run(selected, filtered, chrome, binary))
}

async fn run(cases: Vec<Case>, filtered: usize, chrome: PathBuf, binary: PathBuf) -> ExitCode {
    let started = Instant::now();
    let timeout = std::env::var("E2E_TEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(Duration::from_secs(30), Duration::from_millis);
    println!(
        "\nrunning {} end-to-end tests against {}",
        cases.len(),
        binary.display()
    );
    let fixture = match Fixture::setup(chrome, binary).await {
        Ok(fixture) => Arc::new(fixture),
        Err(error) => {
            eprintln!("global setup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut passed = 0;
    for case in &cases {
        let name = case.full_name();
        crate::expect::set_current_test(&name);
        crate::expect::begin_step("starting".into());
        if let Err(error) = fixture.ensure_browser().await {
            failures.push((
                name.clone(),
                format!("could not launch the browser: {error}"),
            ));
            println!("test {name} ... FAILED");
            continue;
        }
        let begun = Instant::now();
        let handle = tokio::spawn((case.run)(Ctx::new(Arc::clone(&fixture))));
        let abort = handle.abort_handle();
        let failure = match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(error))) => Some(error.to_string()),
            Ok(Err(join)) if join.is_panic() => {
                Some(format!("panicked: {}", panic_text(&*join.into_panic())))
            }
            Ok(Err(join)) => Some(join.to_string()),
            Err(_) => {
                abort.abort();
                Some(format!(
                    "test timeout of {}ms exceeded while: {}",
                    timeout.as_millis(),
                    crate::expect::current_step()
                ))
            }
        };
        fixture.dispose_pages().await;
        let elapsed = begun.elapsed().as_millis();
        match failure {
            None => {
                passed += 1;
                println!("test {name} ... ok ({elapsed} ms)");
            }
            Some(failure) => {
                println!("test {name} ... FAILED ({elapsed} ms)");
                failures.push((name, failure));
            }
        }
    }
    let teardown = fixture.teardown().await;
    match &teardown {
        Ok(summary) => println!("{summary}"),
        Err(error) => println!("[teardown] {error}"),
    }
    if !failures.is_empty() || teardown.is_err() {
        println!("\nfailures:\n");
        for (name, failure) in &failures {
            println!("---- {name} ----\n{failure}\n");
        }
        if let Err(error) = &teardown {
            println!("---- global teardown ----\n{error}\n");
        }
        println!(
            "---- server log (last 80 lines) ----\n{}\n",
            fixture.server().log_tail(80)
        );
        println!("failures:");
        for (name, _) in &failures {
            println!("    {name}");
        }
    }
    let ok = failures.is_empty() && teardown.is_ok();
    println!(
        "\ntest result: {}. {passed} passed; {} failed; 0 ignored; 0 measured; {filtered} filtered out; finished in {:.2}s\n",
        if ok { "ok" } else { "FAILED" },
        failures.len(),
        started.elapsed().as_secs_f64()
    );
    drop(fixture);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_reads_canonical_cards_and_org_options() {
        let html = r#"<select id="org-filter" class="x"><option value="all">All</option><option value="pwtest-1">a</option><option value="pwtest-1-other">b</option></select>
<main><article class="card" data-id="aaa" data-org="pwtest-1"></article>
<article class="card selected" data-org="pwtest-1-other" data-id="bbb"></article>
<article class="cardx" data-id="ccc" data-org="pwtest-1"></article>
<article class="card" data-id="ddd" data-org="other"></article>
<articles class="card" data-id="eee" data-org="pwtest-1"></articles></main>
<article class="card" data-id="fff" data-org="pwtest-1"></article>"#;
        let options = org_filter_options(html);
        assert!(options.contains("pwtest-1") && options.contains("pwtest-1-other"));
        let orgs: HashSet<String> = ["pwtest-1".to_owned(), "pwtest-1-other".to_owned()].into();
        assert_eq!(owned_card_ids(html, &orgs), vec!["aaa", "bbb"]);
        assert_eq!(encode_component("a b/c@d"), "a%20b%2Fc%40d");
        assert_eq!(unique_suffix().len(), 8);
    }
}
