//! U05 cross-runtime proof: publisher auth and Cloudflare Access identity must agree with Node.
//!
//! A Rust-only test can prove Rust is self-consistent; it cannot prove a key hashed by Rust
//! authenticates under Node, that the 60-second leeway lands on the same second as `jose`'s, or
//! that the org-resolution chain has the same precedence. Every assertion here therefore drives
//! the real reference through `node -e`:
//!
//! * `lib/auth.js` — `sha256Hex` and the full `checkKey` extraction matrix, against a real
//!   database seeded through `lib/db.js`'s own `seedKeysFromEnv`, so the hash written by the seed
//!   path and the hash computed on the request path are proven to be the same value.
//! * `lib/identity.js` — `ACCESS_IDENTITY_MODE`, `assertReady()`'s verbatim refusal messages,
//!   `readAccessCookie`, and `orgForEmail` via `createViewerResolver` on the header-trust path.
//! * `lib/access-retry.js` — `accessRetryTarget` over the eligibility and guard matrix.
//! * `jose@5.10.0` — `jwtVerify` with the exact options `lib/identity.js:122-126` passes, over
//!   tokens this suite signs, so the leeway boundaries are compared against the implementation
//!   that actually runs in production rather than against a reading of its source.
//!
//! # Skip visibility
//!
//! These tests **skip** when `node` or the reference sources are unavailable so `cargo test`
//! still works in a Rust-only environment. Set `REQUIRE_NODE_REFERENCE=1` to convert every skip
//! into a hard failure, which is how CI must run this suite and what the U01 contract's M2 delta
//! requires of every cross-runtime proof:
//!
//! ```text
//! REQUIRE_NODE_REFERENCE=1 cargo test
//! ```

use std::sync::Arc;

use artifact_mcp::config::{AppConfig, EnvSource, FixedClock, MapEnv};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{EmailAddress, OrgId};
use artifact_mcp::ports::{BoxFuture, ViewerIdentity};
use artifact_mcp::security::access_retry::{ACCESS_RETRY_PARAM, access_retry_target};
use artifact_mcp::security::auth::{bearer_key, sha256_hex};
use artifact_mcp::security::identity::{
    AccessToken, AccessViewerIdentity, OrgDirectory, TokenRejection, assert_ready,
    read_access_cookie,
};
use artifact_mcp::security::jwks::{JwkDocument, StaticJwks};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
use serde_json::{Value, json};

use crate::u03_support::TempDataDir;
use crate::u05_support::{
    AUDIENCE, KID_CURRENT, KID_ROTATED, NOW_SECONDS, TEAM_DOMAIN, jwks, node_reference_available,
    repo_root, run_node, tamper_signature, token, with_signature,
};

/// The reference modules every case in this file drives.
const NODE_MODULES: [&str; 5] = [
    "lib/auth.js",
    "lib/db.js",
    "lib/identity.js",
    "lib/orgs.js",
    "lib/access-retry.js",
];

/// The seeded publisher key: `clientId:org:secret`.
const SEED_KEYS: &str = "agent-one:acme:s3cret-value,agent-admin:admin:admin-secret";

/// One `node -e` invocation covering every reference entry point this unit ports.
///
/// `lib/identity.js` reads its environment at module load, so each identity case re-imports it
/// with a cache-busting query after rewriting `process.env`. That is the only way to observe
/// several startup modes from one process, and it keeps the database (imported without a query,
/// hence cached) identical across them.
const NODE_DRIVER: &str = r#"
const input = JSON.parse(process.argv[1]);
const url = (p) => `file://${input.root}/${p}`;
const out = {};

(async () => {
  const auth = await import(url("lib/auth.js"));
  const db = await import(url("lib/db.js"));
  db.seedKeysFromEnv(auth.sha256Hex, input.seedKeys);

  out.hashes = input.secrets.map((s) => auth.sha256Hex(s));
  out.checkKey = input.headerCases.map((headers) => {
    const r = auth.checkKey({ headers });
    return r.ok ? { ok: true, clientId: r.clientId, org: r.org, label: r.label } : { ok: false };
  });

  const orgs = await import(url("lib/orgs.js"));
  for (const name of input.orgs) orgs.createOrg({ name });
  for (const [org, domain] of input.orgDomains) orgs.addDomain(org, domain);
  for (const [org, email] of input.orgEmails) orgs.addEmailMember(org, email);

  out.identity = [];
  let n = 0;
  for (const c of input.identityCases) {
    for (const key of input.envKeys) delete process.env[key];
    Object.assign(process.env, c.env);
    const mod = await import(url("lib/identity.js") + `?case=${n++}`);
    const entry = {
      mode: mod.ACCESS_IDENTITY_MODE,
      jwtOn: mod.JWT_VERIFICATION_ON,
      ready: null,
      viewers: [],
      cookies: (c.cookies || []).map((cookie) => mod.readAccessCookie({ headers: { cookie } }))
    };
    try { mod.assertReady(); entry.ready = "ok"; }
    catch (error) { entry.ready = String(error && error.message); }
    const resolve = mod.createViewerResolver({
      verifyJwt: async () => { throw new Error("the header-trust path must not verify a JWT"); }
    });
    for (const email of c.emails || []) {
      entry.viewers.push(await resolve({ headers: { "cf-access-authenticated-user-email": email } }));
    }
    out.identity.push(entry);
  }

  const retry = await import(url("lib/access-retry.js"));
  out.retry = input.retryCases.map((c) => retry.accessRetryTarget(
    { method: c.method, url: c.url, headers: c.headers },
    { mode: c.mode, param: input.retryParam }
  ));

  const jose = await import("jose");
  const keys = jose.createLocalJWKSet(input.jwks);
  out.jose = [];
  for (const c of input.joseCases) {
    try {
      const { payload } = await jose.jwtVerify(c.token, keys, {
        issuer: input.issuer,
        audience: input.audience,
        clockTolerance: c.tolerance,
        currentDate: new Date(c.nowSeconds * 1000)
      });
      out.jose.push({ ok: true, email: payload.email === undefined ? null : payload.email });
    } catch (error) {
      out.jose.push({ ok: false, code: String(error && (error.code || error.name)) });
    }
  }

  process.stdout.write(JSON.stringify(out));
})().catch((error) => { console.error(error); process.exit(1); });
"#;

/// Verify real signed tokens with `jose`, then let the real `lib/identity.js` resolver normalize
/// the verified email and decide whether it belongs to a configured administrator.
const NODE_SIGNED_EMAIL_TRIM_DRIVER: &str = r#"
const input = JSON.parse(process.argv[1]);
const url = (p) => `file://${input.root}/${p}`;

(async () => {
  const identity = await import(url("lib/identity.js"));
  const jose = await import("jose");
  const keys = jose.createLocalJWKSet(input.jwks);
  const resolve = identity.createViewerResolver({
    verifyJwt: (token, _remoteKeys, options) => jose.jwtVerify(token, keys, {
      ...options,
      currentDate: new Date(input.nowSeconds * 1000)
    })
  });
  const viewers = [];
  for (const candidate of input.tokens) {
    viewers.push(await resolve({ headers: { "cf-access-jwt-assertion": candidate } }));
  }
  process.stdout.write(JSON.stringify(viewers));
})().catch((error) => { console.error(error); process.exit(1); });
"#;

// ---------------------------------------------------------------------------
// Case matrices, shared by both runtimes
// ---------------------------------------------------------------------------

/// Secrets whose hash Node and Rust must agree on, including non-ASCII and empty input.
fn secrets() -> Vec<&'static str> {
    vec![
        "",
        "s3cret-value",
        "admin-secret",
        "ünïcødé-🎉-トークン-Ω≈ç√",
        "a key with spaces",
        "CHANGE_ME",
    ]
}

/// Credential header shapes covering every branch of `bearer()`.
fn header_cases() -> Vec<Vec<(&'static str, &'static str)>> {
    vec![
        vec![],
        vec![("authorization", "Bearer s3cret-value")],
        vec![("authorization", "bearer s3cret-value")],
        vec![("authorization", "BEARER s3cret-value")],
        vec![("authorization", "   Bearer \t s3cret-value  ")],
        vec![("authorization", "Bearer admin-secret")],
        vec![("authorization", "Bearer wrong")],
        vec![("x-api-key", "s3cret-value")],
        vec![("x-api-key", "  s3cret-value  ")],
        vec![("x-api-key", "")],
        vec![("x-api-key", "   ")],
        // A matching-but-empty Bearer shadows the fallback.
        vec![
            ("authorization", "Bearer    "),
            ("x-api-key", "s3cret-value"),
        ],
        // A non-matching Authorization falls through to it.
        vec![
            ("authorization", "Basic dXNlcjpwdw=="),
            ("x-api-key", "s3cret-value"),
        ],
        vec![
            ("authorization", "Bearerx nope"),
            ("x-api-key", "s3cret-value"),
        ],
        vec![("authorization", "Bearer"), ("x-api-key", "s3cret-value")],
        vec![
            ("authorization", "Bearer s3cret-value"),
            ("x-api-key", "wrong"),
        ],
    ]
}

/// Cookie header values covering `readAccessCookie`'s parsing rules.
fn cookie_cases() -> Vec<&'static str> {
    vec![
        "",
        "CF_Authorization=token",
        "theme=dark; CF_Authorization=token; view=grid",
        "CF_Authorization=eyJ.part=tail==",
        "  CF_Authorization =  token  ",
        "CF_AuthorizationX=wrong; CF_Authorization=right",
        "CF_AuthorizationX=wrong",
        "cf_authorization=wrongcase",
        "novalue",
        "CF_Authorization=",
        "CF_Authorization=   ",
        "CF_Authorization=; CF_Authorization=real",
        "novalue; CF_Authorization=real",
    ]
}

/// `(method, url, headers, mode)` rows for `accessRetryTarget`.
fn retry_cases() -> Vec<(&'static str, &'static str, bool, bool, &'static str)> {
    // (method, url, has_cookie, has_assertion, mode)
    let mut cases = vec![
        ("GET", "/", true, false, "jwt"),
        ("GET", "/abcdef123456", true, false, "jwt"),
        ("GET", "/abcdef123456?view=grid", true, false, "jwt"),
        ("GET", "/abcdef123456?q=a%20b", true, false, "jwt"),
        ("GET", "/abcdef123456?flag", true, false, "jwt"),
        ("GET", "/abcdef123456?a=1&a=2", true, false, "jwt"),
        ("GET", "/abcdef123456?cf_access_retry=1", true, false, "jwt"),
        ("GET", "//evil.example/abcdef123456", true, false, "jwt"),
        ("GET", "/abcdef123456", false, false, "jwt"),
        ("GET", "/abcdef123456", true, true, "jwt"),
        ("POST", "/abcdef123456", true, false, "jwt"),
        ("HEAD", "/abcdef123456", true, false, "jwt"),
        ("GET", "/abcdef123456", true, false, "header-trust"),
        ("GET", "/abcdef123456", true, false, "disabled"),
    ];
    for path in [
        "/raw/abcdef123456",
        "/raw/abcdef123456/x.css",
        "/thumbnails/abcdef123456",
        "/s/sometoken",
        "/abcdef123456/history",
        "/mcp",
        "/health",
        "/settings",
        "/raw",
        "/short",
        "/ABCDEF123456",
        "/abcdef/second",
        "/favicon.ico",
        "/robots.txt",
    ] {
        cases.push(("GET", path, true, false, "jwt"));
    }
    cases
}

/// One startup-mode case: the environment `lib/identity.js` loads under, plus the addresses to
/// resolve through it.
struct IdentityCase {
    env: Vec<(&'static str, &'static str)>,
    emails: Vec<&'static str>,
}

/// Startup-mode and org-resolution cases; each re-imports `lib/identity.js` with its own env.
fn identity_cases() -> Vec<IdentityCase> {
    let mapping_env = vec![
        ("TRUST_ACCESS_HEADERS", "1"),
        ("LISTEN_HOST", "127.0.0.1"),
        (
            "ORG_EMAIL_DOMAINS",
            "Configured.Test:from-env,registered.test:env-loses,dup.test:first,dup.test:last, spaced.test : spaced-org ,broken,:noorg,nodomain:",
        ),
        ("ADMIN_EMAILS", " Boss@Acme.Test , second@ops.test "),
        ("ADMIN_EMAIL_DOMAINS", "Ops.Test"),
    ];
    let emails = vec![
        "member@acme.test",
        "MEMBER@ACME.TEST",
        "  member@acme.test  ",
        "boss@acme.test",
        "BOSS@acme.test",
        "anyone@ops.test",
        "contractor@shared.test",
        "CONTRACTOR@Shared.Test",
        "user@registered.test",
        "user@configured.test",
        "user@dup.test",
        "user@spaced.test",
        "user@unmapped.test",
        "odd@name@acme.test",
        "nodomain",
        "",
        "   ",
    ];
    let case = |env: Vec<(&'static str, &'static str)>, emails: Vec<&'static str>| IdentityCase {
        env,
        emails,
    };
    vec![
        case(vec![], vec!["member@acme.test"]),
        case(vec![("TRUST_ACCESS_HEADERS", "1")], vec![]),
        case(
            vec![("TRUST_ACCESS_HEADERS", "1"), ("LISTEN_HOST", "127.0.0.1")],
            vec![],
        ),
        case(
            vec![("TRUST_ACCESS_HEADERS", "1"), ("LISTEN_HOST", "::1")],
            vec![],
        ),
        case(
            vec![
                ("TRUST_ACCESS_HEADERS", "1"),
                ("HEADER_TRUST_ALLOW_INSECURE", "1"),
            ],
            vec![],
        ),
        case(vec![("REQUIRE_ACCESS_JWT", "1")], vec![]),
        case(
            vec![
                ("REQUIRE_ACCESS_JWT", "1"),
                ("CF_ACCESS_TEAM_DOMAIN", TEAM_DOMAIN),
            ],
            vec![],
        ),
        case(
            vec![
                ("CF_ACCESS_TEAM_DOMAIN", TEAM_DOMAIN),
                ("CF_ACCESS_AUD", AUDIENCE),
                ("REQUIRE_ACCESS_JWT", "1"),
                ("TRUST_ACCESS_HEADERS", "1"),
            ],
            vec!["member@acme.test"],
        ),
        case(mapping_env, emails),
    ]
}

/// The signed tokens whose accept/reject verdict both runtimes must agree on.
///
/// The leeway rows are the point of this matrix: `nbf` at exactly `now + 60` and `exp` at exactly
/// `now - 59` must be accepted, and one second further out must not.
fn jose_cases() -> Vec<(String, String, i64, u64)> {
    let mut cases: Vec<(String, String, i64, u64)> = Vec::new();
    let mut push = |label: &str, token: String, tolerance: u64| {
        cases.push((label.to_owned(), token, NOW_SECONDS, tolerance));
    };

    push("valid", token(KID_CURRENT, json!({})), 60);
    for offset in [-600_i64, -61, -60, -59, -1, 0, 1, 59, 60, 61, 600] {
        push(
            &format!("nbf{offset:+}"),
            token(
                KID_CURRENT,
                json!({ "nbf": NOW_SECONDS + offset, "exp": NOW_SECONDS + 100_000 }),
            ),
            60,
        );
        push(
            &format!("exp{offset:+}"),
            token(
                KID_CURRENT,
                json!({ "nbf": NOW_SECONDS - 100_000, "exp": NOW_SECONDS + offset }),
            ),
            60,
        );
    }
    for tolerance in [5_u64, 300] {
        for offset in [-1_i64, 0, 1] {
            let boundary = i64::try_from(tolerance).expect("small tolerance") + offset;
            push(
                &format!("nbf@{tolerance}{offset:+}"),
                token(
                    KID_CURRENT,
                    json!({ "nbf": NOW_SECONDS + boundary, "exp": NOW_SECONDS + 100_000 }),
                ),
                tolerance,
            );
            push(
                &format!("exp@{tolerance}{offset:+}"),
                token(
                    KID_CURRENT,
                    json!({ "nbf": NOW_SECONDS - 100_000, "exp": NOW_SECONDS - boundary }),
                ),
                tolerance,
            );
        }
    }

    push(
        "no-temporal-claims",
        token(
            KID_CURRENT,
            json!({ "nbf": null, "exp": null, "iat": null }),
        ),
        60,
    );
    push(
        "wrong-issuer",
        token(KID_CURRENT, json!({ "iss": "https://evil.test" })),
        60,
    );
    push(
        "missing-issuer",
        token(KID_CURRENT, json!({ "iss": null })),
        60,
    );
    push(
        "wrong-audience",
        token(KID_CURRENT, json!({ "aud": "other" })),
        60,
    );
    push(
        "missing-audience",
        token(KID_CURRENT, json!({ "aud": null })),
        60,
    );
    push(
        "audience-array",
        token(KID_CURRENT, json!({ "aud": ["other", AUDIENCE] })),
        60,
    );
    push(
        "audience-array-miss",
        token(KID_CURRENT, json!({ "aud": ["other"] })),
        60,
    );
    push("unknown-kid", token(KID_ROTATED, json!({})), 60);
    push(
        "tampered",
        tamper_signature(&token(KID_CURRENT, json!({}))),
        60,
    );
    push(
        "alg-none",
        with_signature(
            json!({ "alg": "none", "typ": "JWT" }),
            json!({
                "iss": format!("https://{TEAM_DOMAIN}"),
                "aud": AUDIENCE,
                "email": "attacker@evil.test",
                "exp": NOW_SECONDS + 3600
            }),
            "",
        ),
        60,
    );
    push(
        "alg-hs256",
        with_signature(
            json!({ "alg": "HS256", "kid": KID_CURRENT, "typ": "JWT" }),
            json!({
                "iss": format!("https://{TEAM_DOMAIN}"),
                "aud": AUDIENCE,
                "email": "attacker@evil.test",
                "exp": NOW_SECONDS + 3600
            }),
            "AAAA",
        ),
        60,
    );
    push(
        "string-exp",
        token(KID_CURRENT, json!({ "exp": "later" })),
        60,
    );
    push(
        "uppercase-email",
        token(KID_CURRENT, json!({ "email": " Member@ACME.test " })),
        60,
    );
    push("malformed", "not-a-jwt".to_owned(), 60);
    push("two-segments", "a.b".to_owned(), 60);
    cases
}

// ---------------------------------------------------------------------------
// Rust-side evaluation of the same matrices
// ---------------------------------------------------------------------------

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("test header name"),
            HeaderValue::from_str(value).expect("test header value"),
        );
    }
    map
}

/// The seeded key directory, mirrored from [`SEED_KEYS`].
#[derive(Debug)]
struct SeededDirectory;

impl SeededDirectory {
    /// `{ ok, clientId, org, label }` for a request, shaped like Node's `checkKey` result.
    fn check(pairs: &[(&str, &str)]) -> Value {
        let Some(key) = bearer_key(&headers(pairs)) else {
            return json!({ "ok": false });
        };
        let hash = key.hash();
        for (client_id, org, secret) in [
            ("agent-one", "acme", "s3cret-value"),
            ("agent-admin", "admin", "admin-secret"),
        ] {
            if hash.expose() == sha256_hex(secret) {
                return json!({ "ok": true, "clientId": client_id, "org": org, "label": "" });
            }
        }
        json!({ "ok": false })
    }
}

/// A directory mirroring the orgs the Node driver seeds.
#[derive(Debug)]
struct SeededOrgs;

impl OrgDirectory for SeededOrgs {
    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(async move {
            // `org_email_members.email` is `COLLATE NOCASE`. [lib/migrations.js:420]
            Ok(email
                .0
                .eq_ignore_ascii_case("contractor@shared.test")
                .then(|| OrgId::from("acme")))
        })
    }

    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(async move {
            Ok(match domain {
                "acme.test" => Some(OrgId::from("acme")),
                "registered.test" => Some(OrgId::from("from-registry")),
                _ => None,
            })
        })
    }
}

fn config_for(entries: &[(&str, &str)]) -> Arc<AppConfig> {
    let env = entries
        .iter()
        .fold(MapEnv::empty(), |env, (key, value)| env.with(key, value));
    Arc::new(AppConfig::from_source(&env as &dyn EnvSource).expect("test config parses"))
}

fn viewer_json(viewer: &artifact_mcp::model::Viewer) -> Value {
    json!({
        "email": viewer.email.as_ref().map(|e| e.0.clone()),
        "org": viewer.org.as_ref().map(|o| o.0.clone()),
        "isAdmin": viewer.is_admin,
    })
}

fn retry_headers(has_cookie: bool, has_assertion: bool) -> Vec<(&'static str, &'static str)> {
    let mut pairs = Vec::new();
    if has_cookie {
        pairs.push(("cookie", "CF_Authorization=session-token"));
    }
    if has_assertion {
        pairs.push((
            artifact_mcp::security::identity::ACCESS_JWT_HEADER,
            "signed-assertion",
        ));
    }
    pairs
}

fn mode_of(name: &str) -> artifact_mcp::config::AccessIdentityMode {
    use artifact_mcp::config::AccessIdentityMode as Mode;
    match name {
        "jwt" => Mode::Jwt,
        "header-trust" => Mode::HeaderTrust,
        _ => Mode::Disabled,
    }
}

// ---------------------------------------------------------------------------
// The proof
// ---------------------------------------------------------------------------

#[tokio::test]
async fn node_and_rust_agree_on_auth_identity_retry_and_jose_claim_validation() {
    let root = repo_root();
    if !node_reference_available(&root, &NODE_MODULES) {
        return;
    }

    let data_dir = TempDataDir::new("u05-node-parity");
    let key_set = jwks(&[KID_CURRENT]);
    let jose_matrix = jose_cases();
    let identity_matrix = identity_cases();
    let retry_matrix = retry_cases();
    let header_matrix = header_cases();

    let request = json!({
        "root": root.display().to_string(),
        "seedKeys": SEED_KEYS,
        "secrets": secrets(),
        "headerCases": header_matrix
            .iter()
            .map(|pairs| pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), json!(value)))
                .collect::<serde_json::Map<String, Value>>())
            .collect::<Vec<_>>(),
        "orgs": ["acme", "from-registry"],
        "orgDomains": [["acme", "acme.test"], ["from-registry", "registered.test"]],
        "orgEmails": [["acme", "contractor@shared.test"]],
        "envKeys": [
            "CF_ACCESS_TEAM_DOMAIN", "CF_ACCESS_AUD", "TRUST_ACCESS_HEADERS",
            "REQUIRE_ACCESS_JWT", "HEADER_TRUST_ALLOW_INSECURE", "ACCESS_CLOCK_TOLERANCE_S",
            "ORG_EMAIL_DOMAINS", "ADMIN_EMAILS", "ADMIN_EMAIL_DOMAINS", "LISTEN_HOST",
        ],
        "identityCases": identity_matrix
            .iter()
            .enumerate()
            .map(|(index, case)| json!({
                "env": case.env.iter().map(|(k, v)| (String::from(*k), json!(v))).collect::<serde_json::Map<_, _>>(),
                "emails": case.emails,
                // Only the first case needs the cookie matrix; it is parser-only and env-free.
                "cookies": if index == 0 { cookie_cases() } else { Vec::new() },
            }))
            .collect::<Vec<_>>(),
        "retryParam": ACCESS_RETRY_PARAM,
        "retryCases": retry_matrix
            .iter()
            .map(|(method, url, cookie, assertion, mode)| json!({
                "method": method,
                "url": url,
                "mode": mode,
                "headers": retry_headers(*cookie, *assertion)
                    .into_iter()
                    .map(|(k, v)| (String::from(k), json!(v)))
                    .collect::<serde_json::Map<_, _>>(),
            }))
            .collect::<Vec<_>>(),
        "jwks": key_set,
        "issuer": format!("https://{TEAM_DOMAIN}"),
        "audience": AUDIENCE,
        "joseCases": jose_matrix
            .iter()
            .map(|(label, token, now, tolerance)| json!({
                "label": label,
                "token": token,
                "nowSeconds": now,
                "tolerance": tolerance,
            }))
            .collect::<Vec<_>>(),
    });

    let node = run_node(
        &root,
        NODE_DRIVER,
        &request,
        &[
            ("DATA_DIR", &data_dir.path().display().to_string()),
            ("ARTIFACT_API_KEYS", ""),
        ],
    );

    // --- sha256Hex --------------------------------------------------------
    let node_hashes = node["hashes"].as_array().expect("hashes");
    for (secret, expected) in secrets().iter().zip(node_hashes) {
        assert_eq!(
            json!(sha256_hex(secret)),
            *expected,
            "sha256Hex disagreed for {secret:?}"
        );
    }

    // --- checkKey ---------------------------------------------------------
    let node_check = node["checkKey"].as_array().expect("checkKey");
    for (pairs, expected) in header_matrix.iter().zip(node_check) {
        assert_eq!(
            SeededDirectory::check(pairs),
            *expected,
            "checkKey disagreed for {pairs:?}"
        );
    }

    // --- identity ---------------------------------------------------------
    let node_identity = node["identity"].as_array().expect("identity");
    for (case, expected) in identity_matrix.iter().zip(node_identity) {
        let (env, emails) = (&case.env, &case.emails);
        let config = config_for(env);
        assert_eq!(
            json!(config.access.identity_mode().as_str()),
            expected["mode"],
            "ACCESS_IDENTITY_MODE disagreed for {env:?}"
        );
        assert_eq!(
            json!(config.access.jwt_verification_on()),
            expected["jwtOn"],
            "JWT_VERIFICATION_ON disagreed for {env:?}"
        );
        let ready =
            assert_ready(&config).map_or_else(|error| error.to_string(), |()| "ok".to_owned());
        assert_eq!(
            json!(ready),
            expected["ready"],
            "assertReady disagreed for {env:?}"
        );

        let document = JwkDocument::from_json(&key_set).expect("fixture JWKS parses");
        let resolver = AccessViewerIdentity::with_clock(
            Arc::clone(&config),
            Arc::new(StaticJwks::new(document)),
            Arc::new(SeededOrgs),
            Arc::new(FixedClock::from_seconds(NOW_SECONDS)),
        );
        let node_viewers = expected["viewers"].as_array().expect("viewers");
        for (email, node_viewer) in emails.iter().zip(node_viewers) {
            let map = headers(&[(artifact_mcp::security::identity::ACCESS_EMAIL_HEADER, email)]);
            let viewer = resolver
                .resolve(&map)
                .await
                .expect("resolution never errors");
            assert_eq!(
                viewer_json(&viewer),
                *node_viewer,
                "viewer disagreed for {email:?} under {env:?}"
            );
        }

        for (cookie, node_cookie) in cookie_cases()
            .iter()
            .zip(expected["cookies"].as_array().expect("cookies"))
        {
            let rust = read_access_cookie(&headers(&[("cookie", cookie)]))
                .map_or_else(String::new, |t| t.expose().to_owned());
            assert_eq!(
                json!(rust),
                *node_cookie,
                "readAccessCookie disagreed for {cookie:?}"
            );
        }
    }

    // --- accessRetryTarget ------------------------------------------------
    let node_retry = node["retry"].as_array().expect("retry");
    for ((method, url, cookie, assertion, mode), expected) in retry_matrix.iter().zip(node_retry) {
        let rust = access_retry_target(
            &method.parse::<Method>().expect("method"),
            &url.parse::<Uri>().expect("uri"),
            &headers(&retry_headers(*cookie, *assertion)),
            mode_of(mode),
            ACCESS_RETRY_PARAM,
        );
        assert_eq!(
            rust.map_or(Value::Null, Value::from),
            *expected,
            "accessRetryTarget disagreed for {method} {url} (mode {mode})"
        );
    }

    // --- jose claim validation -------------------------------------------
    let node_jose = node["jose"].as_array().expect("jose");
    let mut checked_boundaries = 0_usize;
    for ((label, candidate, _, tolerance), expected) in jose_matrix.iter().zip(node_jose) {
        let config = config_for(&[
            ("CF_ACCESS_TEAM_DOMAIN", TEAM_DOMAIN),
            ("CF_ACCESS_AUD", AUDIENCE),
            ("ACCESS_CLOCK_TOLERANCE_S", &tolerance.to_string()),
        ]);
        let document = JwkDocument::from_json(&key_set).expect("fixture JWKS parses");
        let resolver = AccessViewerIdentity::with_clock(
            config,
            Arc::new(StaticJwks::new(document)),
            Arc::new(SeededOrgs),
            Arc::new(FixedClock::from_seconds(NOW_SECONDS)),
        );
        let rust = resolver.verify(&AccessToken::new(candidate)).await;

        assert_eq!(
            rust.is_ok(),
            expected["ok"].as_bool().expect("ok flag"),
            "verdict disagreed for {label}: rust {rust:?}, node {expected}"
        );
        if let Ok(claims) = &rust {
            // Node reports the raw claim; Rust reports it trimmed and lowercased, which is what
            // `lib/identity.js:127` does immediately afterwards.
            let node_email = expected["email"]
                .as_str()
                .map_or_else(String::new, |email| email.trim().to_lowercase());
            assert_eq!(claims.email, node_email, "email disagreed for {label}");
        }
        // The leeway asymmetry has two different error classes in `jose`; proving the class
        // matches is what rules out an off-by-one that happens to reject on both sides.
        match (&rust, expected["code"].as_str()) {
            (Err(TokenRejection::Expired), Some(code)) => {
                assert_eq!(code, "ERR_JWT_EXPIRED", "{label}");
                checked_boundaries += 1;
            }
            (Err(TokenRejection::NotYetValid), Some(code)) => {
                assert_eq!(code, "ERR_JWT_CLAIM_VALIDATION_FAILED", "{label}");
                checked_boundaries += 1;
            }
            _ => {}
        }
    }
    assert!(
        checked_boundaries >= 8,
        "the leeway matrix must exercise both rejection classes (saw {checked_boundaries})"
    );
}

#[tokio::test]
async fn signed_admin_email_trim_matches_the_node_identity_resolver() {
    let root = repo_root();
    if !node_reference_available(&root, &["lib/identity.js"]) {
        return;
    }

    let cases = [
        (
            "U+0085 NEL",
            token(
                KID_CURRENT,
                json!({ "email": "\u{85}boss@acme.test\u{85}" }),
            ),
            false,
        ),
        (
            "U+FEFF BOM",
            token(
                KID_CURRENT,
                json!({ "email": "\u{feff}boss@acme.test\u{feff}" }),
            ),
            true,
        ),
    ];
    let key_set = jwks(&[KID_CURRENT]);
    let data_dir = TempDataDir::new("u05-signed-trim");
    let request = json!({
        "root": root.display().to_string(),
        "jwks": key_set,
        "nowSeconds": NOW_SECONDS,
        "tokens": cases.iter().map(|(_, candidate, _)| candidate).collect::<Vec<_>>(),
    });
    let node = run_node(
        &root,
        NODE_SIGNED_EMAIL_TRIM_DRIVER,
        &request,
        &[
            ("DATA_DIR", &data_dir.path().display().to_string()),
            ("CF_ACCESS_TEAM_DOMAIN", TEAM_DOMAIN),
            ("CF_ACCESS_AUD", AUDIENCE),
            ("ADMIN_EMAILS", "boss@acme.test"),
        ],
    );
    let node_viewers = node.as_array().expect("node viewers");

    let config = config_for(&[
        ("CF_ACCESS_TEAM_DOMAIN", TEAM_DOMAIN),
        ("CF_ACCESS_AUD", AUDIENCE),
        ("ADMIN_EMAILS", "boss@acme.test"),
    ]);
    let document = JwkDocument::from_json(&key_set).expect("fixture JWKS parses");
    let resolver = AccessViewerIdentity::with_clock(
        config,
        Arc::new(StaticJwks::new(document)),
        Arc::new(SeededOrgs),
        Arc::new(FixedClock::from_seconds(NOW_SECONDS)),
    );

    for ((label, candidate, node_should_be_admin), node_viewer) in cases.iter().zip(node_viewers) {
        assert_eq!(
            node_viewer["isAdmin"].as_bool(),
            Some(*node_should_be_admin),
            "the real Node identity resolver did not exhibit the expected {label} trim behavior"
        );
        let rust_viewer = resolver
            .resolve(&headers(&[(
                artifact_mcp::security::identity::ACCESS_JWT_HEADER,
                candidate,
            )]))
            .await
            .expect("resolution never errors");
        assert_eq!(
            viewer_json(&rust_viewer),
            *node_viewer,
            "signed JWT trim parity diverged for {label}"
        );
    }
}
