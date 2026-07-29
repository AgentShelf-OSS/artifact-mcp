//! U09 cross-runtime proof: the Rust org/key persistence must agree with the real `lib/orgs.js`,
//! `lib/keys.js`, and `seedKeysFromEnv` ([lib/db.js:30-59]) — value for value and, crucially,
//! *message* for message. Admin routes hand a thrown message straight to the client as a 400
//! body, so validation strings are part of the public contract.
//!
//! The test replays one scripted list of operations against both runtimes on their own temporary
//! databases and compares the JSON results step by step. Nothing is re-implemented in the driver:
//! Node runs the actual library modules.
//!
//! # Skip visibility
//!
//! Without `node`/`node_modules` these tests skip so `cargo test` still works in a Rust-only
//! environment. `REQUIRE_NODE_REFERENCE=1` converts every skip into a hard failure, which is how
//! CI must run this suite (U01 contract, "RESOLVED at M2 — cross-runtime skip hazard").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::config::SeededRandom;
use artifact_mcp::error::AppError;
use artifact_mcp::model::{ClientId, CreateOrganization, CreatePublisherKey, OrgId};
use artifact_mcp::persistence::{keys, orgs};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::u09_support::TestDb;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    matches!(std::env::var("REQUIRE_NODE_REFERENCE").as_deref(), Ok("1"))
}

/// Node reference availability; a missing reference is a skip unless `REQUIRE_NODE_REFERENCE=1`.
fn node_reference_available(root: &Path) -> bool {
    let missing = if !root.join("node_modules/better-sqlite3").is_dir() {
        Some("node_modules/better-sqlite3 is missing")
    } else if !root.join("lib/orgs.js").is_file() || !root.join("lib/keys.js").is_file() {
        Some("lib/orgs.js or lib/keys.js is missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };

    match missing {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "REQUIRE_NODE_REFERENCE=1 but the Node reference is unavailable ({reason}); \
                 the U09 org/key parity proof did not run"
            );
            eprintln!("skipping U09 Node parity proof: {reason}");
            eprintln!("set REQUIRE_NODE_REFERENCE=1 to make this a failure instead");
            false
        }
    }
}

/// Drives the real Node modules over a scripted operation list.
///
/// Secrets never cross this boundary: `createKey` results are reduced to the secret's *length*
/// and the hash comparison is done through the deterministic seeding path instead.
const NODE_DRIVER: &str = r#"
const root = process.argv[1];
const load = (name) => import(`file://${root}/lib/${name}`);
Promise.all([load("orgs.js"), load("keys.js"), load("db.js"), load("auth.js")])
  .then(([orgs, keys, dbModule, auth]) => {
    const db = dbModule.default;
    const request = JSON.parse(process.argv[2]);
    const orgValue = (o) => ({
      name: o.name,
      label: o.label,
      color: o.color === undefined ? null : (o.color || null),
      domains: o.domains,
      emails: o.emails,
      categories: o.categories,
      keyCount: o.keyCount
    });
    const run = (s) => {
      switch (s.op) {
        case "createOrg": return orgValue(orgs.createOrg({ name: s.name, label: s.label, domain: s.domain }));
        case "deleteOrg": return orgs.deleteOrg(s.name);
        case "orgExists": return orgs.orgExists(s.name);
        case "addDomain": return orgs.addDomain(s.org, s.domain).domain;
        case "removeDomain": return orgs.removeDomain(s.org, s.domain);
        case "orgForDomain": return orgs.orgForDomain(s.domain);
        case "addEmailMember": return orgs.addEmailMember(s.org, s.email).email;
        case "removeEmailMember": return orgs.removeEmailMember(s.org, s.email);
        case "orgForEmail": return orgs.orgForEmail(s.email);
        case "addCategory": return orgs.addCategory(s.org, s.name).name;
        case "removeCategory": return orgs.removeCategory(s.org, s.name);
        case "categories": return orgs.categoriesFor(s.org);
        case "setColor": return orgs.setColor(s.name, s.color);
        case "colorMap": return orgs.colorMap();
        case "listOrgs": return orgs.listOrgs().map(orgValue);
        case "listOrgNames": return orgs.listOrgNames();
        case "createKey": {
          const created = keys.createKey({ clientId: s.clientId, org: s.org, label: s.label, role: s.role });
          return { clientId: created.clientId, org: created.org, label: created.label, role: created.role, secretLength: created.secret.length };
        }
        case "listKeys": return keys.listKeys().map((k) => ({
          client_id: k.client_id, org: k.org, label: k.label, role: k.role, revoked: Boolean(k.revoked_at)
        }));
        case "revokeKey": return keys.revokeKey(s.clientId);
        case "seed": return dbModule.seedKeysFromEnv(auth.sha256Hex, s.raw);
        case "dumpKeys": return db.prepare("SELECT client_id, org, key_hash FROM api_keys ORDER BY client_id").all();
        default: throw new Error(`unknown op ${s.op}`);
      }
    };
    const results = [];
    for (const step of request.steps) {
      try {
        const value = run(step);
        results.push({ ok: true, value: value === undefined ? null : value });
      } catch (error) {
        results.push({ ok: false, error: String((error && error.message) || error) });
      }
    }
    console.log(JSON.stringify(results));
  })
  .catch((error) => { console.error(error); process.exit(1); });
"#;

fn run_node(root: &Path, data_dir: &Path, steps: &Value) -> Vec<Value> {
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(root.to_string_lossy().as_ref())
        .arg(json!({ "steps": steps }).to_string())
        .env("DATA_DIR", data_dir)
        .env_remove("ORG_EMAIL_DOMAINS")
        .env_remove("ARTIFACT_API_KEYS")
        .env_remove("WEBHOOK_ENC_KEY")
        .output()
        .expect("run the node reference");
    assert!(
        output.status.success(),
        "node reference failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("node stdout is utf-8");
    serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
        panic!("node reference emitted unparseable output ({error}): {stdout}")
    })
}

// ---------------------------------------------------------------------------
// The Rust side of the same script
// ---------------------------------------------------------------------------

fn text(step: &Value, key: &str) -> String {
    step.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn org_value(org: &artifact_mcp::model::Organization) -> Value {
    json!({
        "name": org.name.0,
        "label": org.label,
        "color": org.color,
        "domains": org.domains,
        "emails": org.emails,
        "categories": org.categories,
        "keyCount": org.key_count,
    })
}

fn run_rust(conn: &mut Connection, steps: &Value) -> Vec<Value> {
    let random = SeededRandom::new(0x0009_2026_0721_0001);
    let mut results = Vec::new();
    for step in steps.as_array().expect("steps is an array") {
        let outcome: Result<Value, AppError> = match text(step, "op").as_str() {
            "createOrg" => orgs::create_org(
                conn,
                &CreateOrganization {
                    name: OrgId(text(step, "name")),
                    label: text(step, "label"),
                    domain: step
                        .get("domain")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            )
            .map(|org| org_value(&org)),
            "deleteOrg" => orgs::delete_org(conn, &text(step, "name")).map(Value::from),
            "orgExists" => orgs::org_exists(conn, &text(step, "name")).map(Value::from),
            "addDomain" => {
                orgs::add_domain(conn, &text(step, "org"), &text(step, "domain")).map(Value::from)
            }
            "removeDomain" => orgs::remove_domain(conn, &text(step, "org"), &text(step, "domain"))
                .map(Value::from),
            "orgForDomain" => orgs::org_for_domain(conn, &text(step, "domain"))
                .map(|org| org.map_or(Value::Null, |org| Value::from(org.0))),
            "addEmailMember" => {
                orgs::add_email_member(conn, &text(step, "org"), &text(step, "email"))
                    .map(Value::from)
            }
            "removeEmailMember" => {
                orgs::remove_email_member(conn, &text(step, "org"), &text(step, "email"))
                    .map(Value::from)
            }
            "orgForEmail" => orgs::org_for_email(conn, &text(step, "email"))
                .map(|org| org.map_or(Value::Null, |org| Value::from(org.0))),
            "addCategory" => {
                orgs::add_category(conn, &text(step, "org"), &text(step, "name")).map(Value::from)
            }
            "removeCategory" => {
                orgs::remove_category(conn, &text(step, "org"), &text(step, "name"))
                    .map(Value::from)
            }
            "categories" => orgs::categories(conn, &text(step, "org")).map(Value::from),
            "setColor" => {
                let name = text(step, "name");
                let color = step.get("color").and_then(Value::as_str);
                orgs::set_color(conn, &name, color)
                    .map(|color| json!({ "name": orgs::norm_org(&name), "color": color }))
            }
            "colorMap" => orgs::color_map(conn).map(|map| {
                Value::Object(
                    map.into_iter()
                        .map(|(org, color)| (org.0, color.map_or(Value::Null, Value::from)))
                        .collect(),
                )
            }),
            "listOrgs" => {
                orgs::list_orgs(conn).map(|list| Value::Array(list.iter().map(org_value).collect()))
            }
            "listOrgNames" => orgs::org_names(conn).map(|names| {
                Value::Array(names.into_iter().map(|org| Value::from(org.0)).collect())
            }),
            "createKey" => keys::create_key(
                conn,
                &CreatePublisherKey {
                    client_id: ClientId(text(step, "clientId")),
                    org: OrgId(text(step, "org")),
                    label: text(step, "label"),
                    role: text(step, "role"),
                    owner_email: None,
                },
                &random,
            )
            .map(|created| {
                json!({
                    "clientId": created.client_id.0,
                    "org": created.org.0,
                    "label": created.label,
                    "role": created.role,
                    "secretLength": created.secret.len(),
                })
            }),
            "listKeys" => keys::list_keys(conn).map(|list| {
                Value::Array(
                    list.into_iter()
                        .map(|key| {
                            json!({
                                "client_id": key.client_id.0,
                                "org": key.org.0,
                                "label": key.label,
                                "role": key.role,
                                "revoked": key.revoked_at.is_some(),
                            })
                        })
                        .collect(),
                )
            }),
            "revokeKey" => keys::revoke_key(conn, &text(step, "clientId")).map(Value::from),
            "seed" => keys::seed_keys_from_env(conn, &text(step, "raw")).map(Value::from),
            "dumpKeys" => dump_keys(conn),
            other => panic!("unknown op {other}"),
        };
        results.push(match outcome {
            Ok(value) => json!({ "ok": true, "value": value }),
            Err(AppError::Validation(message)) => json!({ "ok": false, "error": message }),
            Err(error) => panic!("unexpected non-validation error: {error:?}"),
        });
    }
    results
}

fn dump_keys(conn: &Connection) -> Result<Value, AppError> {
    let mut statement = conn
        .prepare("SELECT client_id, org, key_hash FROM api_keys ORDER BY client_id")
        .expect("prepare dump");
    let rows = statement
        .query_map([], |row| {
            Ok(json!({
                "client_id": row.get::<_, String>(0)?,
                "org": row.get::<_, String>(1)?,
                "key_hash": row.get::<_, String>(2)?,
            }))
        })
        .expect("dump keys");
    Ok(Value::Array(
        rows.collect::<rusqlite::Result<Vec<_>>>().expect("rows"),
    ))
}

fn compare(steps: &Value, node: &[Value], rust: &[Value]) {
    let steps = steps.as_array().expect("steps is an array");
    assert_eq!(
        node.len(),
        steps.len(),
        "node returned the wrong step count"
    );
    assert_eq!(
        rust.len(),
        steps.len(),
        "rust returned the wrong step count"
    );
    for (index, step) in steps.iter().enumerate() {
        assert_eq!(
            rust[index], node[index],
            "step {index} diverged\n  step: {step}\n  node: {}\n  rust: {}",
            node[index], rust[index]
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 1 — the org/key registry surface
// ---------------------------------------------------------------------------

fn registry_steps() -> Value {
    Value::Array(vec![
        // --- create: normalization, reserved names, duplicates, malformed ids ---
        json!({ "op": "createOrg", "name": "  ACME  ", "label": "  Acme Incorporated  ", "domain": null }),
        json!({ "op": "createOrg", "name": "acme", "label": "again", "domain": null }),
        json!({ "op": "createOrg", "name": "ACME", "label": "again", "domain": null }),
        json!({ "op": "createOrg", "name": "admin", "label": "", "domain": null }),
        json!({ "op": "createOrg", "name": " Admin ", "label": "", "domain": null }),
        json!({ "op": "createOrg", "name": "", "label": "", "domain": null }),
        json!({ "op": "createOrg", "name": "  ", "label": "", "domain": null }),
        json!({ "op": "createOrg", "name": "-bad", "label": "", "domain": null }),
        json!({ "op": "createOrg", "name": "bad name", "label": "", "domain": null }),
        json!({ "op": "createOrg", "name": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "label": "", "domain": null }),
        json!({ "op": "createOrg", "name": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "label": "", "domain": null }),
        // A label longer than 80 UTF-16 units is truncated.
        json!({ "op": "createOrg", "name": "labelled", "label": "LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL", "domain": null }),
        // --- create with a domain ---
        json!({ "op": "createOrg", "name": "globex", "label": "", "domain": "  Globex.Example  " }),
        json!({ "op": "createOrg", "name": "initech", "label": "", "domain": "globex.example" }),
        json!({ "op": "orgExists", "name": "initech" }),
        json!({ "op": "createOrg", "name": "initech", "label": "", "domain": "not a domain" }),
        json!({ "op": "createOrg", "name": "blank", "label": "", "domain": "   " }),
        json!({ "op": "createOrg", "name": "empty", "label": "", "domain": "" }),
        json!({ "op": "listOrgNames" }),
        // --- domains ---
        json!({ "op": "addDomain", "org": "nope", "domain": "example.com" }),
        json!({ "op": "addDomain", "org": "nope", "domain": "bad domain" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "example" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "-bad.example" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "bad-.example" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "a..b" }),
        json!({ "op": "addDomain", "org": "acme", "domain": ".example.com" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "example.com." }),
        // Unicode case folding must agree between the two runtimes (the message echoes it).
        json!({ "op": "addDomain", "org": "acme", "domain": "İ.CÖM" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "EXAMPLE.\u{212a}OM" }),
        json!({ "op": "addDomain", "org": "acme", "domain": "  Example.COM  " }),
        json!({ "op": "addDomain", "org": "acme", "domain": "EXAMPLE.com" }),
        json!({ "op": "addDomain", "org": "globex", "domain": "example.com" }),
        json!({ "op": "orgForDomain", "domain": "  EXAMPLE.com  " }),
        json!({ "op": "orgForDomain", "domain": "unmapped.example" }),
        json!({ "op": "removeDomain", "org": "acme", "domain": " EXAMPLE.com " }),
        json!({ "op": "removeDomain", "org": "acme", "domain": "example.com" }),
        // --- explicit email members ---
        json!({ "op": "addEmailMember", "org": "nope", "email": "a@b.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "no-at-sign" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "@example.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "a@@example.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "a@example" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": ".a@example.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "a.@example.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "a..b@example.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "a b@example.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "a\u{a0}b@example.com" }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "  Person@Example.COM  " }),
        json!({ "op": "addEmailMember", "org": "acme", "email": "PERSON@example.com" }),
        json!({ "op": "addEmailMember", "org": "globex", "email": "person@example.com" }),
        json!({ "op": "orgForEmail", "email": " PERSON@EXAMPLE.com " }),
        json!({ "op": "orgForEmail", "email": "nobody@example.com" }),
        json!({ "op": "removeEmailMember", "org": "acme", "email": "PERSON@Example.com" }),
        json!({ "op": "removeEmailMember", "org": "acme", "email": "person@example.com" }),
        // --- categories ---
        json!({ "op": "addCategory", "org": "nope", "name": "Docs" }),
        json!({ "op": "addCategory", "org": "nope", "name": "   " }),
        json!({ "op": "addCategory", "org": "acme", "name": "" }),
        json!({ "op": "addCategory", "org": "acme", "name": "   " }),
        json!({ "op": "addCategory", "org": "acme", "name": "\u{feff}" }),
        // U+0085 is Unicode whitespace but NOT JavaScript whitespace: it must survive.
        json!({ "op": "addCategory", "org": "acme", "name": "\u{85}kept" }),
        json!({ "op": "addCategory", "org": "acme", "name": "  Design   Docs \n" }),
        json!({ "op": "addCategory", "org": "acme", "name": "Design Docs" }),
        json!({ "op": "addCategory", "org": "acme", "name": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" }),
        json!({ "op": "categories", "org": "acme" }),
        json!({ "op": "removeCategory", "org": "acme", "name": " Design    Docs " }),
        json!({ "op": "removeCategory", "org": "acme", "name": "Design Docs" }),
        json!({ "op": "removeCategory", "org": "acme", "name": "design docs" }),
        // --- colors ---
        json!({ "op": "setColor", "name": "nope", "color": "#abc" }),
        json!({ "op": "setColor", "name": "acme", "color": "356B9F" }),
        json!({ "op": "setColor", "name": "acme", "color": "#abcd" }),
        json!({ "op": "setColor", "name": "acme", "color": "#gggggg" }),
        json!({ "op": "setColor", "name": "acme", "color": " #356B9F " }),
        json!({ "op": "setColor", "name": "acme", "color": "#abc" }),
        json!({ "op": "setColor", "name": "acme", "color": "  " }),
        json!({ "op": "setColor", "name": "acme", "color": null }),
        json!({ "op": "setColor", "name": "globex", "color": "#000000" }),
        json!({ "op": "colorMap" }),
        // --- keys ---
        json!({ "op": "createKey", "clientId": "", "org": "acme", "label": "" }),
        json!({ "op": "createKey", "clientId": "a", "org": "acme", "label": "" }),
        json!({ "op": "createKey", "clientId": ".ab", "org": "acme", "label": "" }),
        json!({ "op": "createKey", "clientId": "a b", "org": "acme", "label": "" }),
        json!({ "op": "createKey", "clientId": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "org": "acme", "label": "" }),
        json!({ "op": "createKey", "clientId": "pub", "org": "", "label": "" }),
        json!({ "op": "createKey", "clientId": "pub", "org": "-acme", "label": "" }),
        json!({ "op": "createKey", "clientId": "pub", "org": "ac me", "label": "" }),
        json!({ "op": "createKey", "clientId": "  publisher-1  ", "org": "  acme  ", "label": "  LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL  " }),
        json!({ "op": "createKey", "clientId": "publisher-1", "org": "acme", "label": "" }),
        json!({ "op": "createKey", "clientId": " publisher-1 ", "org": "acme", "label": "" }),
        json!({ "op": "createKey", "clientId": "PUBLISHER-1", "org": "a", "label": "" }),
        json!({ "op": "createKey", "clientId": "zeta", "org": "beta", "label": "" }),
        json!({ "op": "createKey", "clientId": "role-invalid", "org": "acme", "label": "", "role": "owner" }),
        json!({ "op": "createKey", "clientId": "role-reader", "org": "acme", "label": "", "role": "reader" }),
        json!({ "op": "createKey", "clientId": "role-collaborator", "org": "acme", "label": "", "role": "collaborator" }),
        json!({ "op": "createKey", "clientId": "role-default", "org": "acme", "label": "", "role": "" }),
        json!({ "op": "revokeKey", "clientId": " publisher-1 " }),
        json!({ "op": "revokeKey", "clientId": "missing" }),
        json!({ "op": "revokeKey", "clientId": "publisher-1" }),
        json!({ "op": "revokeKey", "clientId": "publisher-1" }),
        json!({ "op": "listKeys" }),
        // --- org listing reflects every mutation above, including active key counts ---
        json!({ "op": "listOrgs" }),
        // --- delete cascades ---
        json!({ "op": "deleteOrg", "name": " acme " }),
        json!({ "op": "deleteOrg", "name": "acme" }),
        json!({ "op": "listOrgs" }),
        json!({ "op": "listKeys" }),
    ])
}

#[test]
fn org_and_key_operations_match_the_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let steps = registry_steps();
    let node_dir = crate::u03_support::TempDataDir::new("u09-node");
    let node = run_node(&root, node_dir.path(), &steps);

    let db = TestDb::new("u09-rust");
    let mut conn = db.conn();
    let rust = run_rust(&mut conn, &steps);

    compare(&steps, &node, &rust);
    // A silently empty script would make the comparison vacuous.
    assert!(steps.as_array().is_some_and(|list| list.len() > 80));
}

// ---------------------------------------------------------------------------
// Scenario 2 — `ARTIFACT_API_KEYS` seeding, hashes included
// ---------------------------------------------------------------------------

fn seed_steps() -> Value {
    Value::Array(vec![
        json!({ "op": "seed", "raw": "" }),
        json!({ "op": "seed", "raw": "   " }),
        json!({ "op": "seed", "raw": "\u{feff}" }),
        json!({ "op": "seed", "raw": "onlyone" }),
        json!({ "op": "seed", "raw": ":acme:orphan" }),
        json!({ "op": "seed", "raw": "noSecret:acme:" }),
        json!({ "op": "seed", "raw": "full:acme:s1" }),
        json!({ "op": "seed", "raw": "short:s2" }),
        json!({ "op": "seed", "raw": " spaced : acme : a : b " }),
        json!({ "op": "seed", "raw": "empty::s4" }),
        json!({ "op": "seed", "raw": "placeholder1:acme:CHANGE_ME" }),
        json!({ "op": "seed", "raw": "placeholder2:acme:REPLACE_WITH_LONG_RANDOM_SECRET" }),
        json!({ "op": "seed", "raw": "placeholder3:acme:CHANGE_ME_NOW" }),
        json!({ "op": "seed", "raw": "placeholder4:CHANGE_ME" }),
        // Repeating an existing client id must not change or duplicate the row.
        json!({ "op": "seed", "raw": "full:other:different-secret" }),
        // A multi-entry value, including skipped fragments.
        json!({ "op": "seed", "raw": "multi1:acme:m1,,multi2:m2, multi3 : org3 : m:3 " }),
        json!({ "op": "dumpKeys" }),
        // Revoked rows are still conflicts: seeding must not resurrect them.
        json!({ "op": "revokeKey", "clientId": "full" }),
        json!({ "op": "seed", "raw": "full:acme:s1-again" }),
        json!({ "op": "dumpKeys" }),
        json!({ "op": "listKeys" }),
    ])
}

#[test]
fn environment_seeding_and_hashing_match_the_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let steps = seed_steps();
    let node_dir = crate::u03_support::TempDataDir::new("u09-node-seed");
    let node = run_node(&root, node_dir.path(), &steps);

    let db = TestDb::new("u09-rust-seed");
    let mut conn = db.conn();
    let rust = run_rust(&mut conn, &steps);

    compare(&steps, &node, &rust);

    // `dumpKeys` compares `key_hash` values directly, so equality here is a byte-level proof
    // that both runtimes hash the same secret string with the same algorithm.
    let hashes: Vec<&Value> = node
        .iter()
        .filter_map(|step| step.get("value"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|row| row.get("key_hash"))
        .collect();
    assert!(
        hashes.len() >= 8,
        "expected the seeded key hashes to be part of the comparison, saw {}",
        hashes.len()
    );
    assert!(
        hashes
            .iter()
            .all(|hash| hash.as_str().is_some_and(|value| value.len() == 64)),
        "seeded hashes are 64-character sha256 hex digests"
    );
}

// ---------------------------------------------------------------------------
// Normalization helpers, proven against Node's own string semantics
// ---------------------------------------------------------------------------

/// JavaScript's trim/`\s`/`toLowerCase`/`slice` behaviour differs from Rust's in ways that change
/// what gets stored. This pins the four primitives against the real engine rather than against a
/// second Rust implementation.
#[test]
fn javascript_string_primitives_agree_with_the_engine() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }

    let samples = [
        " padded ",
        "\u{feff}zwnbsp\u{feff}",
        "\u{85}nel\u{85}",
        "\u{3000}ideographic\u{3000}",
        "\u{a0}nbsp\u{a0}",
        "\n\ttabs\r\n",
        "MiXeD CaSe",
        "İSTANBUL",
        "\u{212a}elvin",
        "STRASSE",
        "no  collapse",
    ];
    let script = r#"
const values = JSON.parse(process.argv[1]);
console.log(JSON.stringify(values.map((value) => ({
  trimmed: value.trim(),
  lowered: value.toLowerCase(),
  length: value.length,
  collapsed: value.trim().replace(/\s+/g, " ").slice(0, 60)
}))));
"#;
    let output = Command::new("node")
        .current_dir(&root)
        .arg("-e")
        .arg(script)
        .arg(serde_json::to_string(&samples).expect("encode samples"))
        .output()
        .expect("run node string probe");
    assert!(output.status.success(), "node string probe failed");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let node: Vec<BTreeMap<String, Value>> =
        serde_json::from_str(stdout.trim()).expect("parse node output");

    for (sample, expected) in samples.iter().zip(node.iter()) {
        assert_eq!(
            orgs::js_trim(sample),
            expected["trimmed"].as_str().expect("string"),
            "trim({sample:?})"
        );
        assert_eq!(
            sample.to_lowercase(),
            expected["lowered"].as_str().expect("string"),
            "toLowerCase({sample:?})"
        );
        assert_eq!(
            u64::try_from(orgs::utf16_len(sample)).expect("length fits u64"),
            expected["length"].as_u64().expect("number"),
            "length({sample:?})"
        );
        assert_eq!(
            orgs::norm_category(sample),
            expected["collapsed"].as_str().expect("string"),
            "normCategory({sample:?})"
        );
    }
}
