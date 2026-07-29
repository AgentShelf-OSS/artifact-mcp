//! U09 publisher-key persistence: validation, uniqueness, revocation, the `ARTIFACT_API_KEYS`
//! bootstrap matrix, and the guarantee that a raw secret never reaches storage, logs, or errors.
//!
//! Node oracle: `lib/keys.js` and `seedKeysFromEnv` ([lib/db.js:30-59]).

use std::sync::Arc;

use artifact_mcp::config::{RandomSource, SeededRandom};
use artifact_mcp::model::{ClientId, CreatePublisherKey, OrgId};
use artifact_mcp::persistence::keys::{self, KeyStore};
use artifact_mcp::persistence::orgs;

use crate::u09_support::{TestDb, seed_org, validation_message};

fn request(client_id: &str, org: &str, label: &str) -> CreatePublisherKey {
    CreatePublisherKey {
        client_id: ClientId(client_id.to_owned()),
        org: OrgId(org.to_owned()),
        label: label.to_owned(),
        role: "author".to_owned(),
        owner_email: None,
    }
}

fn request_with_role(client_id: &str, role: &str) -> CreatePublisherKey {
    CreatePublisherKey {
        client_id: ClientId(client_id.to_owned()),
        org: OrgId("acme".to_owned()),
        label: String::new(),
        role: role.to_owned(),
        owner_email: None,
    }
}

fn random() -> SeededRandom {
    SeededRandom::new(0x00A9_0000_0000_0001)
}

/// Every text value stored in `api_keys`, used to prove no column holds a raw secret.
fn all_key_cells(conn: &rusqlite::Connection) -> Vec<String> {
    let mut statement = conn
        .prepare("SELECT client_id, org, label, role, key_hash, created_at, COALESCE(revoked_at, '') FROM api_keys")
        .expect("prepare dump");
    let rows = statement
        .query_map([], |row| {
            (0..7)
                .map(|index| row.get::<_, String>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .expect("dump keys");
    rows.flat_map(|row| row.expect("read row")).collect()
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

#[test]
fn creates_a_key_storing_only_its_hash() {
    let db = TestDb::new("u09-key-create");
    let conn = db.conn();
    let entropy = random();

    let created = keys::create_key(
        &conn,
        &request(
            "  publisher-1  ",
            "  acme  ",
            &format!("  {}  ", "L".repeat(80)),
        ),
        &entropy,
    )
    .expect("create key");

    assert_eq!(created.client_id, ClientId("publisher-1".to_owned()));
    assert_eq!(created.org, OrgId("acme".to_owned()));
    assert_eq!(created.label.len(), keys::KEY_LABEL_MAX_LENGTH);
    assert_eq!(created.role, "author");
    assert_eq!(created.secret.len(), keys::SECRET_BYTES * 2);
    assert!(
        created
            .secret
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );

    let stored: String = conn
        .query_row(
            "SELECT key_hash FROM api_keys WHERE client_id = 'publisher-1'",
            [],
            |row| row.get(0),
        )
        .expect("read hash");
    assert_eq!(
        stored,
        keys::key_hash(&artifact_mcp::config::Secret::new(created.secret.clone())),
        "the stored hash must be sha256Hex(secret)"
    );
    assert_eq!(stored.len(), 64);
    assert_ne!(stored, created.secret);
    assert_eq!(
        conn.query_row(
            "SELECT role FROM api_keys WHERE client_id = 'publisher-1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read role"),
        "author"
    );
}

#[test]
fn key_roles_are_validated_persisted_and_default_to_author() {
    let db = TestDb::new("u09-key-roles");
    let conn = db.conn();
    let entropy = random();

    for role in ["reader", "author", "collaborator"] {
        let client_id = format!("key-{role}");
        let created = keys::create_key(&conn, &request_with_role(&client_id, role), &entropy)
            .expect("create role key");
        assert_eq!(created.role, role);
    }
    let defaulted = keys::create_key(&conn, &request_with_role("key-default", ""), &entropy)
        .expect("default role");
    assert_eq!(defaulted.role, "author");

    assert_eq!(
        validation_message(keys::create_key(
            &conn,
            &request_with_role("key-invalid", "owner"),
            &entropy,
        )),
        keys::INVALID_KEY_ROLE_MESSAGE
    );
    let listed = keys::list_keys(&conn).expect("list role keys");
    assert!(
        listed.iter().any(|key| {
            key.client_id == ClientId("key-reader".to_owned()) && key.role == "reader"
        })
    );
    assert!(listed.iter().any(|key| {
        key.client_id == ClientId("key-collaborator".to_owned()) && key.role == "collaborator"
    }));
}

#[test]
fn rejects_malformed_and_duplicate_client_ids_in_node_order() {
    let db = TestDb::new("u09-key-invalid");
    let conn = db.conn();
    let entropy = random();

    for bad in ["", "a", ".ab", "a b", &format!("a{}", "b".repeat(41))] {
        assert_eq!(
            validation_message(keys::create_key(&conn, &request(bad, "acme", ""), &entropy)),
            "Name must be 2–41 characters: letters, numbers, dot, dash, underscore.",
            "input {bad:?}"
        );
    }
    for bad in ["", "-acme", "ac me", &format!("a{}", "b".repeat(41))] {
        assert_eq!(
            validation_message(keys::create_key(&conn, &request("pub", bad, ""), &entropy)),
            "Org must be letters, numbers, dot, dash, or underscore.",
            "input {bad:?}"
        );
    }
    // A single-character org is valid even though a single-character name is not.
    keys::create_key(&conn, &request("pub", "a", ""), &entropy).expect("create");

    assert_eq!(
        validation_message(keys::create_key(
            &conn,
            &request(" pub ", "other", ""),
            &entropy
        )),
        "A key named \"pub\" already exists."
    );
    // The client id is compared case sensitively, so "PUB" is a different key.
    keys::create_key(&conn, &request("PUB", "a", ""), &entropy).expect("create");
}

#[test]
fn the_key_row_never_contains_a_raw_secret_and_neither_do_errors() {
    let db = TestDb::new("u09-key-leak");
    let conn = db.conn();
    let entropy = random();

    let created =
        keys::create_key(&conn, &request("pub", "acme", "label"), &entropy).expect("create key");
    let secret = created.secret.clone();
    assert!(!secret.is_empty());

    for cell in all_key_cells(&conn) {
        assert!(
            !cell.contains(&secret),
            "a stored column leaked the raw secret: {cell}"
        );
    }

    // Every failing path returns a message built only from caller-supplied identifiers.
    let duplicate = validation_message(keys::create_key(
        &conn,
        &request("pub", "acme", ""),
        &entropy,
    ));
    assert!(!duplicate.contains(&secret));

    // A seeded secret is likewise absent from every persisted cell.
    let seeded_secret = "S3CRET-seed-value";
    keys::seed_keys_from_env(&conn, &format!("seeded:acme:{seeded_secret}")).expect("seed");
    for cell in all_key_cells(&conn) {
        assert!(
            !cell.contains(seeded_secret),
            "seeding leaked the raw secret: {cell}"
        );
    }
}

// ---------------------------------------------------------------------------
// list and revoke
// ---------------------------------------------------------------------------

#[test]
fn lists_active_keys_first_then_by_org_and_client_id() {
    let db = TestDb::new("u09-key-list");
    let conn = db.conn();
    let entropy = random();

    for (client_id, org) in [
        ("zeta", "beta"),
        ("alpha", "beta"),
        ("gamma", "alpha"),
        ("revoked", "alpha"),
    ] {
        keys::create_key(&conn, &request(client_id, org, "l"), &entropy).expect("create");
    }
    assert!(keys::revoke_key(&conn, "revoked").expect("revoke"));

    let listed = keys::list_keys(&conn).expect("list");
    assert_eq!(
        listed
            .iter()
            .map(|key| (key.org.0.as_str(), key.client_id.0.as_str()))
            .collect::<Vec<_>>(),
        [
            ("alpha", "gamma"),
            ("beta", "alpha"),
            ("beta", "zeta"),
            ("alpha", "revoked"),
        ]
    );
    assert!(listed[3].revoked_at.is_some());
    assert!(listed.iter().take(3).all(|key| key.revoked_at.is_none()));
    assert!(listed.iter().all(|key| !key.created_at.0.is_empty()));
}

#[test]
fn revokes_once_and_never_matches_an_untrimmed_id() {
    let db = TestDb::new("u09-key-revoke");
    let conn = db.conn();
    keys::create_key(&conn, &request("pub", "acme", ""), &random()).expect("create");

    // `revokeKey` passes the id through untrimmed. [lib/keys.js:44]
    assert!(!keys::revoke_key(&conn, " pub ").expect("revoke"));
    assert!(!keys::revoke_key(&conn, "missing").expect("revoke"));
    assert!(keys::revoke_key(&conn, "pub").expect("revoke"));
    assert!(!keys::revoke_key(&conn, "pub").expect("revoke again"));
}

// ---------------------------------------------------------------------------
// ARTIFACT_API_KEYS bootstrap seeding
// ---------------------------------------------------------------------------

fn seeded_rows(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
    let mut statement = conn
        .prepare("SELECT client_id, org, key_hash FROM api_keys ORDER BY client_id")
        .expect("prepare");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("query");
    rows.collect::<rusqlite::Result<Vec<_>>>().expect("rows")
}

fn hash_of(secret: &str) -> String {
    keys::key_hash(&artifact_mcp::config::Secret::new(secret))
}

#[test]
fn seeds_three_part_two_part_and_colon_bearing_entries() {
    let db = TestDb::new("u09-seed");
    let conn = db.conn();

    let raw = "full:acme:s1, short:s2 ,  spaced : acme : a : b ,,onlyone,:acme:s3,empty::s4";
    assert_eq!(keys::seed_keys_from_env(&conn, raw).expect("seed"), 4);
    assert_eq!(
        seeded_rows(&conn),
        [
            // Each part is trimmed before the secret is rejoined on ":". [lib/db.js:41-44]
            ("empty".to_owned(), "default".to_owned(), hash_of("s4")),
            ("full".to_owned(), "acme".to_owned(), hash_of("s1")),
            ("short".to_owned(), "default".to_owned(), hash_of("s2")),
            ("spaced".to_owned(), "acme".to_owned(), hash_of("a:b")),
        ],
        "`onlyone` (one part) and `:acme:s3` (empty client id) are skipped"
    );
}

#[test]
fn refuses_documented_placeholder_secrets() {
    let db = TestDb::new("u09-seed-placeholder");
    let conn = db.conn();

    let raw = "a:acme:CHANGE_ME,b:acme:REPLACE_WITH_LONG_RANDOM_SECRET,c:acme:real-secret";
    assert_eq!(keys::seed_keys_from_env(&conn, raw).expect("seed"), 1);
    assert_eq!(
        seeded_rows(&conn),
        [("c".to_owned(), "acme".to_owned(), hash_of("real-secret"))]
    );
    // The refusal is by exact value: a placeholder embedded in a longer secret still seeds.
    assert_eq!(
        keys::seed_keys_from_env(&conn, "d:acme:CHANGE_ME_NOW").expect("seed"),
        1
    );
}

#[test]
fn seeding_never_touches_an_existing_row() {
    let db = TestDb::new("u09-seed-conflict");
    let conn = db.conn();
    keys::create_key(&conn, &request("pub", "acme", "managed"), &random()).expect("create");
    assert!(keys::revoke_key(&conn, "pub").expect("revoke"));
    let before = seeded_rows(&conn);

    // Same client id, different org and secret: `ON CONFLICT(client_id) DO NOTHING` means the
    // Settings-managed (and revoked) row stays authoritative. [lib/db.js:28-29]
    assert_eq!(
        keys::seed_keys_from_env(&conn, "pub:other:brand-new").expect("seed"),
        0
    );
    assert_eq!(seeded_rows(&conn), before);
    let revoked: Option<String> = conn
        .query_row(
            "SELECT revoked_at FROM api_keys WHERE client_id = 'pub'",
            [],
            |row| row.get(0),
        )
        .expect("read");
    assert!(revoked.is_some(), "a seeded entry must not un-revoke a key");
}

#[test]
fn an_empty_or_blank_environment_value_seeds_nothing() {
    let db = TestDb::new("u09-seed-empty");
    let conn = db.conn();
    for raw in ["", "   ", "\u{feff}", "\n\t"] {
        assert_eq!(keys::seed_keys_from_env(&conn, raw).expect("seed"), 0);
    }
    assert!(seeded_rows(&conn).is_empty());
}

// ---------------------------------------------------------------------------
// pooled adapter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_pooled_store_creates_lists_revokes_and_seeds() {
    let db = TestDb::new("u09-key-store");
    let entropy: Arc<dyn RandomSource> = Arc::new(random());
    let store = KeyStore::with_random(db.pool().clone(), entropy);

    let created = store
        .create_key(request("pub", "acme", "label"))
        .await
        .expect("create");
    assert_eq!(created.client_id, ClientId("pub".to_owned()));
    assert_eq!(store.list_keys().await.expect("list").len(), 1);

    assert_eq!(
        validation_message(store.create_key(request("pub", "acme", "")).await),
        "A key named \"pub\" already exists."
    );

    assert_eq!(
        store.seed_from_env("seeded:acme:s1").await.expect("seed"),
        1
    );
    assert_eq!(store.list_keys().await.expect("list").len(), 2);

    let client_id = ClientId("pub".to_owned());
    assert!(store.revoke_key(&client_id).await.expect("revoke"));
    assert!(!store.revoke_key(&client_id).await.expect("revoke"));
}

#[test]
fn key_owner_changes_are_verified_and_legacy_backfill_is_previewed_then_null_only() {
    let db = TestDb::new("u09-owner-management");
    let mut conn = db.conn();
    seed_org(&mut conn, "acme");
    orgs::add_email_member(&conn, "acme", "one@acme.test").expect("member one");
    orgs::add_email_member(&conn, "acme", "two@acme.test").expect("member two");
    keys::create_key(&conn, &request("publisher", "acme", "managed"), &random()).expect("key");

    let assigned = keys::set_key_owner(&conn, "publisher", Some("one@acme.test"))
        .expect("set")
        .expect("key exists");
    assert_eq!(assigned.owner_email.as_deref(), Some("one@acme.test"));
    assert_eq!(
        validation_message(keys::set_key_owner(
            &conn,
            "publisher",
            Some("foreign@other.test")
        )),
        "Owner must be a verified member of this organization."
    );

    conn.execute(
        "INSERT INTO artifacts (id, client_id, org, title, owner_email) VALUES ('legacy-a', 'publisher', 'acme', 'A', NULL), ('legacy-b', 'publisher', 'acme', 'B', NULL), ('attributed', 'publisher', 'acme', 'C', 'one@acme.test'), ('other-key', 'other', 'acme', 'D', NULL)",
        [],
    )
    .expect("seed artifacts");
    let preview = keys::backfill_key_owner(&mut conn, "publisher", "two@acme.test", false)
        .expect("preview")
        .expect("key exists");
    assert_eq!(
        (preview.matched, preview.updated, preview.confirmed),
        (2, 0, false)
    );
    let confirmed = keys::backfill_key_owner(&mut conn, "publisher", "two@acme.test", true)
        .expect("confirm")
        .expect("key exists");
    assert_eq!(
        (confirmed.matched, confirmed.updated, confirmed.confirmed),
        (2, 2, true)
    );
    let owners: Vec<(String, Option<String>)> = conn
        .prepare("SELECT id, owner_email FROM artifacts ORDER BY id")
        .expect("query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows")
        .collect::<rusqlite::Result<_>>()
        .expect("collect");
    assert_eq!(
        owners,
        vec![
            ("attributed".to_owned(), Some("one@acme.test".to_owned())),
            ("legacy-a".to_owned(), Some("two@acme.test".to_owned())),
            ("legacy-b".to_owned(), Some("two@acme.test".to_owned())),
            ("other-key".to_owned(), None),
        ]
    );
}
