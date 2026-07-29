//! Shared fixtures for the U05 suites: real signing keys, a real JWKS, and the Node oracle seam.
//!
//! Every JWT in these suites is genuinely signed with a private key that lives in this file, so
//! "the signature verifies" and "the signature was tampered with" are both real outcomes rather
//! than mocked ones. The keys are throwaway RSA-2048 pairs generated for the test suite; they
//! authenticate nothing.

#![allow(
    dead_code,
    reason = "each U05 suite uses a different subset of these fixtures"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, EncodingKey};
use serde_json::{Value, json};

/// Setting this to `1` turns "Node is unavailable" from a skip into a failure.
///
/// Mandated for every cross-runtime proof by the U01 contract's M2 delta.
pub const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

/// The Access team domain every fixture token is issued for.
pub const TEAM_DOMAIN: &str = "u05.cloudflareaccess.test";
/// The Access application audience tag every fixture token targets.
pub const AUDIENCE: &str = "a5c0ffee1234567890abcdefa5c0ffee1234567890abcdefa5c0ffee12345678";
/// `kid` of the key Cloudflare is currently publishing.
pub const KID_CURRENT: &str = "u05-current";
/// `kid` of the key Cloudflare rotates to.
pub const KID_ROTATED: &str = "u05-rotated";

/// The instant every deterministic assertion is evaluated at (2026-01-01T00:00:00Z).
pub const NOW_SECONDS: i64 = 1_767_225_600;

/// Throwaway RSA-2048 private key backing [`KID_CURRENT`]. Test-only; grants nothing.
pub const PRIVATE_KEY_CURRENT: &str = concat!(
    "-----BEGIN ",
    "PRIVATE KEY-----\n",
    "\
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC+n6Vu8IThrwVi
N0ysrpd94iGjevMLzDUgK76Ln8Seotk0YQ5LtmciCE8E3ssJ4PhsCnJbrdaDuja0
Vo4M2YLH2emaWyhAZI5v2NzIO1e2nYhzYZMw+Rkhah5zrxwVORXAT+v4JDxtcw05
qrppAn0z6iGkFnTgOGalW3sgtbIhoDoH7yW41hhz/WCQ1nzlyEOHApgiGaQ2MSnM
cOdmMxkueOblxe2guPQN9A3r2vZm3GUzJtVLWOdbuBHI3Oe6Fep2LEEFeJaCrf4D
nO3dz7zJmItEwIWdHGD5u282VJDSQR/IQBGrm2Q0pTqiFfogkfaShQS7wDV4JGzO
IYWIeTJ3AgMBAAECggEAG7cpMyjJST73RhZ1kXiOZ4Ekvu42FEsL67IwhtXN8qVe
y0d+mpzaAJAQsnaU3XTWPxnYaA1YboKj8t3QKCd1OmAr9Ni4Jin4rmPA22QKwW8a
KHHRbYIU7fojP1KrKfLI1NNJ+HmG8lWTYK+6trXj76kPscSKGGewP5k+HRK4jubK
tG0ZJshaxQc51BJMGNMmYcBhuj6Qdipudh4OwJbzVwuvLf/yAiM35GloOREXDBxq
IyrRApfZfYCP0ja8qHeTi4T4WVtyP4weFgxPueJg9m01G91ixDH34NQ3ErOkgcmW
vZE2ZR6C+tH0NHcZpsUKYrclVySoSP/XbPTY7v1WwQKBgQDg2HVcIpCoHPjOjlX5
OeM7ywEzZi3ux0UWF19IsTsa6EjTl+49AtvEYhf0lB0eODGMQP5I1Y5ouEoDsE3F
yMTQghgYE+9tCoyEHW7+MAtmTV+Xr+QXyrt4a6q7uuARe0VQL6+bby7CoUF2+tDu
FdyOUvIo/OQdz3KbAHfWa12T8QKBgQDZCUuI98Fk4ClWFLtda2ZiyXQyvvUuuVXy
/MCIMYdjuK6uZxo6gptF/4pNCebJH3CPDpNBwHlfxwkJvZxTz1n8HszcPYl+t1im
x4FfAejMrOG4W3Ya7HR3EXTzatUcqo2b7Yji9sfLeCPd2IHnr69XGKXlaN6mr7rs
vN99wXf05wKBgDDo5R6VpkHri3PjInCEVxm6nxg/Md6vGigkDWYSp3jC7pSYiApd
hNDDRdK+JVddgemweZ/+VGwTKoaC42gStD1nDzatn3doxAg5HtvMN66ZRiII8OT8
BKu9P/Z2QCeNWRaISPrWxKUxzrvC84/W0ZNkF6ky2axiY9uzzl2mHUUBAoGAFbpR
fo/XI6MxYDXJICwdXuxuHppxb1EMorvdBoV22WvmyPz3aj4jD1nq3ZWNLjtgiGHc
Kiv7urPxWrFJ0jYi+xOWTMI4XqA6VtAISpulU8BHBK7bXwynCDiOLcRXO3xzmtWn
65jrHZqUdKQ1NR2ofx6vlQzvpV5YZ1TtRui0eqcCgYEA2/4ihIFTua7Tp7pcfxl9
KdQOdsUWCleXLd9QiKJmnqN5/4VLtGx5UWBX7QLJpleXTTmJPJ5sLQJI+EKnord5
SYIewQUty/7mf8h6W/+TclSa0j9k8cDC3/ygeiGaNiEVG6+JozO+1YB/DKpbHPHe
mlpWANfLNU+6eCY6wm8R/Is=
-----END PRIVATE KEY-----
",
);

/// Throwaway RSA-2048 private key backing [`KID_ROTATED`]. Test-only; grants nothing.
pub const PRIVATE_KEY_ROTATED: &str = concat!(
    "-----BEGIN ",
    "PRIVATE KEY-----\n",
    "\
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCcy/wP7absk4y7
rMnRWa5sobmStNttUZ2CUadVOuSuUepUQ9rPjTub57sJ/eGqPcVpA525t0G9ZH1J
F2bsvf3tpoMy5a6UUSh9c5KkYH6hNnNnZelmDvZgUEbBq9BUzvFirwW1Nw/oEAVp
OJ9mFnh9ZYR9cV4QT2SuNscv1cdEnC+IKkiV0MlnUjUC9VW0cL+cf4uxGpwL3c8X
tA0GU4STSFRDVOs/6ZxJk2J0FZ8vNCkNOYd7nz1+ZgQ8OGE+mKlcAddbaykJkL7X
lnOPDR80zP3irlsndasLaAYorIOuGXDGaKsVGnYsMdWBfmKowzJ8WrpOITtOh8bw
lfU2mNShAgMBAAECggEABjN3iyo5Ei1mMRRs8WGAeVWejRenv2+tQOaJjYB8I4xE
DhPWayOw5jIwlp0yr+zMOkJdOiduG0dGgK3jIU0ExkGx+eD1SfKZPTflRmi2DVpl
z0Krq8B+/DFkkPuCbF44U/Cr0z5Dm+gJTL9jZ3JPTtdFWYAyqga3yr+KGmFUJYmP
ZZ34Q+yCw2pNPSGbQ8CHUNnvKUG5+Uo+tXqc/yFbDtYvee+bNGo8y5bNWm3CmPx+
oBG9q85l7HD5znU2VlWDPx6x4htpt8mdQb6eRS9O0xaWoTiwWf/yA2UFjN2UbpJo
0cwFCjeRxR0+K3w1cBZhDQ/qUrjC+wo8I5XF0XN/aQKBgQDTOx6I8XoL1qpWVjLz
hYcIj8RSz5g2cZG8jTgaIZfpIC/CTZzM5q04FNReg/DIRnkuQmKmy58LCb+MFqlP
Cw3R84N+cQpm8Fel3itCCthtdNhGbnbuhdQDmT48eGrzWEDmHZfzB9uyUZavU7VH
QkcPUhwjAs7RYX8JToOVfgPyuQKBgQC+B2bAlBOCRfEniW1dcaPqMypQD4j3jwpa
cr9co9Sp9lAVyzDDyVuuSdlBxSSaMMMEtmHo95oxtL/tdIlAZYKcYUCHewW4wd1y
YBDODLLSrN1lzVmN+O1lrklAaLimupUay01PGO0h/Q0HHiejQRGvu6/azW5SMPd8
2GRJ5TgdKQKBgHp0VnJbUz6TtvIQTL9iVHMBLXY4hOxjEHK3h6OWgAOVNjq1VcZv
oFHXuXoFkUv5lvzbXWeGue/jOdlTtdlt9hgVzNA7ZiVhBd7RmlBSC0ABMfQ6y9Xh
XZSsfSj/QjlKm20MEO/CSXnp1KpVo8zovltCZa9iTFWT6NqTWrMKd+15AoGBALcT
ZGAWiPESNzJTCUVkbXn9zz8QqHFwopXfRROYVxNj1WYZuyJ1BNnWFfRyXUAbyFbq
60tJ+Ij4zYuUoYKkCYBlhYjA8hM82v8NJEOPIl0r46TngObxsq0qizH9ciBXU71b
rmCM8DC1ne6Ek8WJs+NtXA/dqPKQcG8b/wreRgB5AoGAOplpTuFr6EbpJQlbG1N6
cLuonU3CogKMB1ynve/jTvLULf1F2E7Kflsm+CewgaWEjMX1VP8N3zlLo+vTdFSi
A2vn0jskCRUcrRvNQ7z/IDMNdENuENkMB92EcnPchr/JKbcpZt0VclfEZRnEtFix
XI3QpBcfnYaxTShUU3NDvlE=
-----END PRIVATE KEY-----
",
);

/// The private key for a `kid`.
#[must_use]
pub fn private_key_pem(kid: &str) -> &'static str {
    match kid {
        KID_CURRENT => PRIVATE_KEY_CURRENT,
        KID_ROTATED => PRIVATE_KEY_ROTATED,
        other => panic!("no fixture key for kid {other}"),
    }
}

/// The signing key for a `kid`.
#[must_use]
pub fn signing_key(kid: &str) -> EncodingKey {
    EncodingKey::from_rsa_pem(private_key_pem(kid).as_bytes()).expect("fixture key parses")
}

/// The public JWK for a `kid`, derived from the private key rather than transcribed.
///
/// Deriving it means the key set can never silently disagree with the signer.
#[must_use]
pub fn jwk(kid: &str) -> Value {
    let key = Jwk::from_encoding_key(&signing_key(kid), Algorithm::RS256)
        .expect("fixture key exports as a JWK");
    let mut value = serde_json::to_value(key).expect("JWK serializes");
    let members = value.as_object_mut().expect("JWK is an object");
    members.insert("kid".to_owned(), json!(kid));
    members.insert("use".to_owned(), json!("sig"));
    value
}

/// A `{ "keys": [...] }` document for the given `kid`s, in order.
#[must_use]
pub fn jwks(kids: &[&str]) -> Value {
    json!({ "keys": kids.iter().map(|kid| jwk(kid)).collect::<Vec<Value>>() })
}

/// A valid, signed Cloudflare Access assertion.
///
/// `claims` is merged over the standard `iss`/`aud`/`email`/`iat`/`nbf`/`exp` set, so a test
/// overrides only the claim it is about.
#[must_use]
pub fn token(kid: &str, claims: Value) -> String {
    let mut payload = json!({
        "iss": format!("https://{TEAM_DOMAIN}"),
        "aud": AUDIENCE,
        "email": "member@acme.test",
        "iat": NOW_SECONDS,
        "nbf": NOW_SECONDS,
        "exp": NOW_SECONDS + 3600,
    });
    merge(&mut payload, claims);
    signed(
        json!({ "alg": "RS256", "kid": kid, "typ": "JWT" }),
        payload,
        kid,
    )
}

/// A signed token with a fully explicit header, for algorithm and `crit` probes.
#[must_use]
pub fn signed(header: Value, payload: Value, signing_kid: &str) -> String {
    let message = format!("{}.{}", encode_part(&header), encode_part(&payload));
    let signature = jsonwebtoken::crypto::sign(
        message.as_bytes(),
        &signing_key(signing_kid),
        Algorithm::RS256,
    )
    .expect("fixture token signs");
    format!("{message}.{signature}")
}

/// An unsigned compact token whose signature segment is `signature`.
#[must_use]
pub fn with_signature(header: Value, payload: Value, signature: &str) -> String {
    format!(
        "{}.{}.{signature}",
        encode_part(&header),
        encode_part(&payload)
    )
}

/// Flip one character of the signature so the compact form stays well-shaped but no longer
/// verifies.
#[must_use]
pub fn tamper_signature(token: &str) -> String {
    let (message, signature) = token
        .rsplit_once('.')
        .expect("compact token has a signature");
    let mut bytes = signature.as_bytes().to_vec();
    let last = bytes.len() - 1;
    bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
    format!(
        "{message}.{}",
        String::from_utf8(bytes).expect("base64url stays ASCII")
    )
}

/// Replace the payload with `claims` while keeping the original (now invalid) signature.
#[must_use]
pub fn tamper_payload(token: &str, claims: Value) -> String {
    let mut parts = token.split('.');
    let header = parts.next().expect("header segment");
    let signature = parts.nth(1).expect("signature segment");
    format!("{header}.{}.{signature}", encode_part(&claims))
}

fn encode_part(value: &Value) -> String {
    BASE64URL.encode(serde_json::to_vec(value).expect("fixture JSON serializes"))
}

fn merge(target: &mut Value, patch: Value) {
    let (Some(target), Value::Object(patch)) = (target.as_object_mut(), patch) else {
        return;
    };
    for (key, value) in patch {
        if value.is_null() {
            target.remove(&key);
        } else {
            target.insert(key, value);
        }
    }
}

// ---------------------------------------------------------------------------
// Node oracle
// ---------------------------------------------------------------------------

/// The repository root, which is also the Node reference's working directory.
#[must_use]
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Whether `REQUIRE_NODE_REFERENCE=1` is set.
#[must_use]
pub fn require_node_reference() -> bool {
    std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1")
}

/// Node reference availability, following `tests/native/u04_crypto.rs`.
///
/// Returns `false` (skip) only when `REQUIRE_NODE_REFERENCE=1` is unset; otherwise it fails, so a
/// CI job cannot green-pass without ever having run the cross-runtime proof. The U01 contract
/// requires this of every cross-runtime proof added after M2.
#[must_use]
pub fn node_reference_available(root: &Path, modules: &[&str]) -> bool {
    let missing = modules
        .iter()
        .find(|module| !root.join(module).is_file())
        .map(|module| format!("{module} is missing"));
    let unavailable = missing.or_else(|| match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => None,
        _ => Some("node is not on PATH".to_owned()),
    });

    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the Node/Rust parity proof did not run"
            );
            eprintln!("skipping U05 Node parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

/// Run a `node -e` driver with `request` as `process.argv[1]` and parse its JSON stdout.
///
/// `env` supplies the module-load-time environment `lib/identity.js` reads. `DATA_DIR` must be
/// among it: importing any `lib/` module transitively imports `lib/db.js`, which opens a database
/// on import.
#[must_use]
pub fn run_node(root: &Path, driver: &str, request: &Value, env: &[(&str, &str)]) -> Value {
    let mut command = Command::new("node");
    command
        .current_dir(root)
        .arg("-e")
        .arg(driver)
        .arg(request.to_string());
    // Start from a clean slate so a developer's shell cannot change what the oracle reports.
    for key in [
        "CF_ACCESS_TEAM_DOMAIN",
        "CF_ACCESS_AUD",
        "TRUST_ACCESS_HEADERS",
        "REQUIRE_ACCESS_JWT",
        "HEADER_TRUST_ALLOW_INSECURE",
        "ACCESS_CLOCK_TOLERANCE_S",
        "ORG_EMAIL_DOMAINS",
        "ADMIN_EMAILS",
        "ADMIN_EMAIL_DOMAINS",
        "ARTIFACT_API_KEYS",
        "LISTEN_HOST",
    ] {
        command.env_remove(key);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("run the node reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "node reference emitted non-JSON ({error}):\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
