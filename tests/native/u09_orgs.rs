//! U09 organization persistence: validation order, normalization, uniqueness, cascade, and the
//! `COLLATE NOCASE` membership semantics introduced by migration v21.
//!
//! Node oracle: `lib/orgs.js`. Messages asserted here are the literal 400 bodies the admin routes
//! return ([lib/app.js:308-310] and siblings); `u09_node_parity.rs` proves them against the real
//! Node implementation.

use std::collections::BTreeMap;

use artifact_mcp::error::AppError;
use artifact_mcp::model::{CreateOrganization, OrgId};
use artifact_mcp::persistence::orgs::{self, OrgStore};

use crate::u09_support::{TestDb, seed_org, validation_message};

fn request(name: &str, label: &str, domain: Option<&str>) -> CreateOrganization {
    CreateOrganization {
        name: OrgId(name.to_owned()),
        label: label.to_owned(),
        domain: domain.map(str::to_owned),
    }
}

// ---------------------------------------------------------------------------
// create_org
// ---------------------------------------------------------------------------

#[test]
fn creates_an_org_case_folded_with_a_capped_label() {
    let db = TestDb::new("u09-create");
    let mut conn = db.conn();

    let long_label = "L".repeat(100);
    let created =
        orgs::create_org(&mut conn, &request("  ACME  ", &long_label, None)).expect("create org");

    assert_eq!(created.name, OrgId("acme".to_owned()));
    assert_eq!(created.label.len(), orgs::ORG_LABEL_MAX_LENGTH);
    // Node returns a literal, so neither field is read back from the row. [lib/orgs.js:126]
    assert_eq!(created.color, None);
    assert_eq!(created.created_at, None);
    assert_eq!(created.key_count, 0);
    assert!(created.domains.is_empty());
    assert!(orgs::org_exists(&conn, "acme").expect("exists"));
    // The lookups do not case-fold, so the original casing never resolves.
    assert!(!orgs::org_exists(&conn, "ACME").expect("exists"));
}

#[test]
fn rejects_malformed_reserved_and_duplicate_org_names_in_node_order() {
    let db = TestDb::new("u09-create-invalid");
    let mut conn = db.conn();

    for bad in ["", "  ", "-acme", "ac me", &format!("a{}", "b".repeat(41))] {
        assert_eq!(
            validation_message(orgs::create_org(&mut conn, &request(bad, "", None))),
            "Org name must be letters, numbers, dot, dash, or underscore (max 41).",
            "input {bad:?}"
        );
    }
    // Case folding happens before the reserved check, so "ADMIN" is reserved too.
    for reserved in ["admin", "ADMIN", " Admin "] {
        assert_eq!(
            validation_message(orgs::create_org(&mut conn, &request(reserved, "", None))),
            "\"admin\" is a reserved org name."
        );
    }

    orgs::create_org(&mut conn, &request("acme", "", None)).expect("create");
    assert_eq!(
        validation_message(orgs::create_org(&mut conn, &request("ACME", "", None))),
        "Organization \"acme\" already exists."
    );
    // A malformed name is reported before the duplicate check.
    assert_eq!(
        validation_message(orgs::create_org(&mut conn, &request("ac me", "", None))),
        "Org name must be letters, numbers, dot, dash, or underscore (max 41)."
    );
}

#[test]
fn rejects_new_domain_shaped_org_names() {
    let db = TestDb::new("u09-domain-shaped-org");
    let mut conn = db.conn();

    assert_eq!(
        validation_message(orgs::create_org(
            &mut conn,
            &request("tenant.example", "", None)
        )),
        "Org name must not be an email domain. Use a tenant id such as \"acme\" and add the domain separately."
    );
    assert!(!orgs::org_exists(&conn, "tenant.example").expect("exists"));
}

#[test]
fn creates_an_org_with_a_domain_atomically() {
    let db = TestDb::new("u09-create-domain");
    let mut conn = db.conn();

    let created = orgs::create_org(
        &mut conn,
        &request("acme", "Acme", Some("  ACME.Example  ")),
    )
    .expect("create with domain");
    assert_eq!(created.domains, ["acme.example"]);
    assert_eq!(
        orgs::org_for_domain(&conn, "ACME.EXAMPLE").expect("lookup"),
        Some(OrgId("acme".to_owned()))
    );

    // A malformed domain aborts the whole create: no org row is left behind.
    assert_eq!(
        validation_message(orgs::create_org(
            &mut conn,
            &request("globex", "", Some("not a domain"))
        )),
        "\"not a domain\" is not a valid email domain."
    );
    assert!(!orgs::org_exists(&conn, "globex").expect("exists"));

    // A taken domain always reports the cross-org form here. [lib/orgs.js:120]
    assert_eq!(
        validation_message(orgs::create_org(
            &mut conn,
            &request("globex", "", Some("acme.example"))
        )),
        "Domain \"acme.example\" is already mapped to \"acme\"."
    );
    assert!(!orgs::org_exists(&conn, "globex").expect("exists"));

    // An empty or whitespace-only domain is simply absent. [lib/orgs.js:116]
    let blank = orgs::create_org(&mut conn, &request("blank", "", Some("   "))).expect("create");
    assert!(blank.domains.is_empty());
}

// ---------------------------------------------------------------------------
// domains
// ---------------------------------------------------------------------------

#[test]
fn adds_and_removes_domains_with_node_conflict_messages() {
    let db = TestDb::new("u09-domains");
    let mut conn = db.conn();
    seed_org(&mut conn, "acme");
    seed_org(&mut conn, "globex");

    assert_eq!(
        validation_message(orgs::add_domain(&conn, "nope", "example.com")),
        "Unknown organization \"nope\"."
    );
    // The unknown org is reported before the domain is validated.
    assert_eq!(
        validation_message(orgs::add_domain(&conn, "nope", "bad domain")),
        "Unknown organization \"nope\"."
    );
    for bad in ["", "example", "-bad.example", "bad-.example", "a..b"] {
        assert_eq!(
            validation_message(orgs::add_domain(&conn, "acme", bad)),
            format!("\"{}\" is not a valid email domain.", bad.to_lowercase()),
            "input {bad:?}"
        );
    }

    assert_eq!(
        orgs::add_domain(&conn, "acme", " Example.COM ").expect("add"),
        "example.com"
    );
    assert_eq!(
        validation_message(orgs::add_domain(&conn, "acme", "EXAMPLE.com")),
        "\"example.com\" is already on this org."
    );
    assert_eq!(
        validation_message(orgs::add_domain(&conn, "globex", "example.com")),
        "Domain \"example.com\" is already mapped to \"acme\"."
    );

    assert!(orgs::remove_domain(&conn, "acme", " EXAMPLE.com ").expect("remove"));
    assert!(!orgs::remove_domain(&conn, "acme", "example.com").expect("remove"));
    assert_eq!(
        orgs::org_for_domain(&conn, "example.com").expect("lookup"),
        None
    );
}

#[test]
fn refuses_to_remove_a_legacy_same_name_domain_mapping() {
    let db = TestDb::new("u09-legacy-domain-org");
    let conn = db.conn();
    conn.execute(
        "INSERT INTO orgs (name, label) VALUES ('legacy.example', 'Legacy')",
        [],
    )
    .expect("insert legacy org");
    conn.execute(
        "INSERT INTO org_domains (domain, org) VALUES ('legacy.example', 'legacy.example')",
        [],
    )
    .expect("insert legacy domain mapping");

    assert_eq!(
        validation_message(orgs::remove_domain(
            &conn,
            "legacy.example",
            "legacy.example"
        )),
        "Cannot remove domain \"legacy.example\" from organization \"legacy.example\": implicit domain access would remain. Migrate to a non-domain organization first."
    );
    assert_eq!(
        orgs::org_for_domain(&conn, "legacy.example").expect("lookup"),
        Some(OrgId("legacy.example".to_owned()))
    );
}

// ---------------------------------------------------------------------------
// explicit email membership (v21, COLLATE NOCASE)
// ---------------------------------------------------------------------------

#[test]
fn adds_and_removes_explicit_email_members() {
    let db = TestDb::new("u09-emails");
    let mut conn = db.conn();
    seed_org(&mut conn, "acme");
    seed_org(&mut conn, "globex");

    assert_eq!(
        validation_message(orgs::add_email_member(&conn, "nope", "a@b.com")),
        "Unknown organization \"nope\"."
    );
    for bad in [
        "",
        "no-at-sign",
        "@example.com",
        "a@@example.com",
        "a@example",
        ".a@example.com",
        "a.@example.com",
        "a..b@example.com",
    ] {
        assert_eq!(
            validation_message(orgs::add_email_member(&conn, "acme", bad)),
            format!("\"{bad}\" is not a valid email address."),
            "input {bad:?}"
        );
    }

    assert_eq!(
        orgs::add_email_member(&conn, "acme", "  Person@Example.COM  ").expect("add"),
        "person@example.com"
    );
    assert_eq!(
        validation_message(orgs::add_email_member(&conn, "acme", "PERSON@example.com")),
        "\"person@example.com\" is already on this org."
    );
    assert_eq!(
        validation_message(orgs::add_email_member(
            &conn,
            "globex",
            "person@example.com"
        )),
        "Email \"person@example.com\" is already mapped to \"acme\"."
    );
    assert_eq!(
        orgs::org_for_email(&conn, " PERSON@EXAMPLE.com ").expect("lookup"),
        Some(OrgId("acme".to_owned()))
    );

    assert!(orgs::remove_email_member(&conn, "acme", "PERSON@Example.com").expect("remove"));
    assert!(!orgs::remove_email_member(&conn, "acme", "person@example.com").expect("remove"));
}

#[test]
fn membership_matching_survives_a_row_that_normalization_never_touched() {
    // The v21 column is `email TEXT PRIMARY KEY COLLATE NOCASE`, so case insensitivity is a
    // property of the schema as well as of `normEmail`. A row written by an older path (or by
    // hand) with mixed case must still resolve — and must still collide.
    let db = TestDb::new("u09-nocase");
    let mut conn = db.conn();
    seed_org(&mut conn, "acme");
    seed_org(&mut conn, "globex");
    conn.execute(
        "INSERT INTO org_email_members (email, org) VALUES (?, ?)",
        rusqlite::params!["Mixed.Case@Example.COM", "acme"],
    )
    .expect("insert raw row");

    assert_eq!(
        orgs::org_for_email(&conn, "mixed.case@example.com").expect("lookup"),
        Some(OrgId("acme".to_owned()))
    );
    assert_eq!(
        validation_message(orgs::add_email_member(
            &conn,
            "globex",
            "mixed.case@example.com"
        )),
        "Email \"mixed.case@example.com\" is already mapped to \"acme\"."
    );
    assert!(orgs::remove_email_member(&conn, "acme", "mixed.case@example.com").expect("remove"));
}

// ---------------------------------------------------------------------------
// categories
// ---------------------------------------------------------------------------

#[test]
fn registers_categories_idempotently_after_normalizing_them() {
    let db = TestDb::new("u09-categories");
    let mut conn = db.conn();
    seed_org(&mut conn, "acme");

    assert_eq!(
        validation_message(orgs::add_category(&conn, "nope", "Docs")),
        "Unknown organization \"nope\"."
    );
    // The unknown org is reported before the empty name. [lib/orgs.js:172-173]
    assert_eq!(
        validation_message(orgs::add_category(&conn, "nope", "   ")),
        "Unknown organization \"nope\"."
    );
    for blank in ["", "   ", "\u{feff}"] {
        assert_eq!(
            validation_message(orgs::add_category(&conn, "acme", blank)),
            "Category name is required."
        );
    }

    assert_eq!(
        orgs::add_category(&conn, "acme", "  Design   Docs \n").expect("add"),
        "Design Docs"
    );
    // `INSERT OR IGNORE`: re-adding succeeds and does not duplicate the row.
    assert_eq!(
        orgs::add_category(&conn, "acme", "Design Docs").expect("re-add"),
        "Design Docs"
    );
    orgs::add_category(&conn, "acme", "Alpha").expect("add");
    assert_eq!(
        orgs::categories(&conn, "acme").expect("list"),
        ["Alpha", "Design Docs"]
    );
    assert_eq!(
        orgs::add_category(&conn, "acme", &"x".repeat(80))
            .expect("add")
            .len(),
        orgs::CATEGORY_MAX_LENGTH
    );

    assert!(orgs::remove_category(&conn, "acme", " Design    Docs ").expect("remove"));
    assert!(!orgs::remove_category(&conn, "acme", "Design Docs").expect("remove"));
    // Category matching is case sensitive; only whitespace is normalized.
    assert!(!orgs::remove_category(&conn, "acme", "alpha").expect("remove"));
}

// ---------------------------------------------------------------------------
// color
// ---------------------------------------------------------------------------

#[test]
fn stores_clears_and_validates_org_colors() {
    let db = TestDb::new("u09-color");
    let mut conn = db.conn();
    seed_org(&mut conn, "acme");
    seed_org(&mut conn, "globex");

    assert_eq!(
        validation_message(orgs::set_color(&conn, "nope", Some("#abc"))),
        "Unknown organization \"nope\"."
    );
    for bad in ["356B9F", "#abcd", "#gggggg", "#", "red"] {
        assert_eq!(
            validation_message(orgs::set_color(&conn, "acme", Some(bad))),
            "Color must be a hex value like #356B9F.",
            "input {bad:?}"
        );
    }

    assert_eq!(
        orgs::set_color(&conn, "acme", Some(" #356B9F ")).expect("set"),
        Some("#356B9F".to_owned())
    );
    assert_eq!(
        orgs::set_color(&conn, "acme", Some("#abc")).expect("set"),
        Some("#abc".to_owned())
    );
    // An empty (or missing) color clears the override rather than failing validation.
    assert_eq!(
        orgs::set_color(&conn, "acme", Some("  ")).expect("clear"),
        None
    );
    assert_eq!(orgs::set_color(&conn, "acme", None).expect("clear"), None);

    orgs::set_color(&conn, "globex", Some("#000000")).expect("set");
    let map = orgs::color_map(&conn).expect("color map");
    assert_eq!(
        map,
        BTreeMap::from([
            (OrgId("acme".to_owned()), None),
            (OrgId("globex".to_owned()), Some("#000000".to_owned())),
        ])
    );
}

// ---------------------------------------------------------------------------
// listing
// ---------------------------------------------------------------------------

#[test]
fn lists_orgs_with_their_members_and_active_key_counts() {
    let db = TestDb::new("u09-list");
    let mut conn = db.conn();
    seed_org(&mut conn, "zeta");
    seed_org(&mut conn, "acme");
    orgs::add_domain(&conn, "acme", "acme.example").expect("domain");
    orgs::add_email_member(&conn, "acme", "person@example.com").expect("email");
    orgs::add_category(&conn, "acme", "Docs").expect("category");
    orgs::set_color(&conn, "acme", Some("#abc")).expect("color");
    conn.execute(
        "INSERT INTO api_keys (client_id, org, label, key_hash) VALUES ('live', 'acme', 'l', 'h1'),
         ('dead', 'acme', 'd', 'h2')",
        [],
    )
    .expect("insert keys");
    conn.execute(
        "UPDATE api_keys SET revoked_at = datetime('now') WHERE client_id = 'dead'",
        [],
    )
    .expect("revoke");

    let listed = orgs::list_orgs(&conn).expect("list orgs");
    assert_eq!(
        listed
            .iter()
            .map(|org| org.name.0.as_str())
            .collect::<Vec<_>>(),
        ["acme", "zeta"]
    );
    let acme = &listed[0];
    assert_eq!(acme.domains, ["acme.example"]);
    assert_eq!(acme.emails, ["person@example.com"]);
    assert_eq!(acme.categories, ["Docs"]);
    assert_eq!(acme.color, Some("#abc".to_owned()));
    assert!(acme.created_at.is_some(), "listed rows carry created_at");
    // Only unrevoked keys count. [lib/orgs.js:15]
    assert_eq!(acme.key_count, 1);
    assert_eq!(listed[1].key_count, 0);

    assert_eq!(
        orgs::org_names(&conn).expect("names"),
        [OrgId("acme".to_owned()), OrgId("zeta".to_owned())]
    );
}

// ---------------------------------------------------------------------------
// delete and cascade
// ---------------------------------------------------------------------------

#[test]
fn org_deletion_refuses_artifacts_then_revokes_keys_and_cascades_registry_rows() {
    let db = TestDb::new("u09-cascade");
    let mut conn = db.conn();
    seed_org(&mut conn, "acme");
    orgs::add_domain(&conn, "acme", "acme.example").expect("domain");
    orgs::add_email_member(&conn, "acme", "person@example.com").expect("email");
    orgs::add_category(&conn, "acme", "Docs").expect("category");
    conn.execute(
        "INSERT INTO org_webhooks (id, org, url) VALUES ('wh1', 'acme', 'https://example.invalid/x')",
        [],
    )
    .expect("insert webhook");
    conn.execute(
        "INSERT INTO api_keys (client_id, org, key_hash) VALUES ('k1', 'acme', 'hash')",
        [],
    )
    .expect("insert key");
    conn.execute(
        "INSERT INTO artifacts (id, client_id, org, title) VALUES ('abcdef', 'k1', 'acme', 't')",
        [],
    )
    .expect("insert artifact");

    assert_eq!(
        orgs::delete_org(&mut conn, " acme ").expect_err("artifact must block deletion"),
        AppError::Validation(
            "Cannot delete organization \"acme\" while it owns 1 artifact. Move its artifacts to another organization first."
                .to_owned()
        )
    );

    for (table, expected) in [
        ("orgs", 1_i64),
        ("org_domains", 1),
        ("org_email_members", 1),
        ("org_categories", 1),
        ("org_webhooks", 1),
        ("api_keys", 1),
        ("artifacts", 1),
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rows");
        assert_eq!(count, expected, "table {table}");
    }
    let revoked: Option<String> = conn
        .query_row(
            "SELECT revoked_at FROM api_keys WHERE client_id = 'k1'",
            [],
            |row| row.get(0),
        )
        .expect("key revocation state");
    assert_eq!(revoked, None);

    conn.execute("DELETE FROM artifacts WHERE id = 'abcdef'", [])
        .expect("move/delete artifact before offboarding");
    assert!(orgs::delete_org(&mut conn, " acme ").expect("delete"));
    assert!(!orgs::delete_org(&mut conn, "acme").expect("second delete"));

    for table in [
        "orgs",
        "org_domains",
        "org_email_members",
        "org_categories",
        "org_webhooks",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rows");
        assert_eq!(count, 0, "table {table}");
    }
    let revoked: Option<String> = conn
        .query_row(
            "SELECT revoked_at FROM api_keys WHERE client_id = 'k1'",
            [],
            |row| row.get(0),
        )
        .expect("key revocation state");
    assert!(
        revoked.is_some(),
        "the key row remains as revoked audit history"
    );
    let violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .expect("foreign key check");
    assert_eq!(violations, 0);
}

#[test]
fn org_deletion_rolls_key_revocation_back_if_the_registry_delete_fails() {
    let db = TestDb::new("u09-delete-rollback");
    let mut conn = db.conn();
    seed_org(&mut conn, "rollback-org");
    conn.execute(
        "INSERT INTO api_keys (client_id, org, key_hash) VALUES ('rollback-key', 'rollback-org', 'hash')",
        [],
    )
    .expect("insert key");
    conn.execute_batch(
        "CREATE TRIGGER block_rollback_org_delete
         BEFORE DELETE ON orgs WHEN OLD.name = 'rollback-org'
         BEGIN SELECT RAISE(ABORT, 'blocked delete'); END;",
    )
    .expect("install blocking trigger");

    assert_eq!(
        orgs::delete_org(&mut conn, "rollback-org").expect_err("trigger must abort deletion"),
        AppError::Internal
    );
    assert!(orgs::org_exists(&conn, "rollback-org").expect("org still exists"));
    let revoked: Option<String> = conn
        .query_row(
            "SELECT revoked_at FROM api_keys WHERE client_id = 'rollback-key'",
            [],
            |row| row.get(0),
        )
        .expect("key revocation state");
    assert_eq!(revoked, None, "revocation must roll back with the delete");
}

// ---------------------------------------------------------------------------
// pooled adapter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_pooled_store_exposes_the_same_behaviour_as_the_sync_layer() {
    let db = TestDb::new("u09-store");
    let store = OrgStore::new(db.pool().clone());
    let acme = OrgId("acme".to_owned());

    let created = store
        .create_org(request("Acme", "Acme Inc", Some("acme.example")))
        .await
        .expect("create");
    assert_eq!(created.name, acme);
    assert!(store.org_exists(&acme).await.expect("exists"));
    assert_eq!(
        store.org_for_domain("ACME.example").await.expect("domain"),
        Some(acme.clone())
    );

    let email = artifact_mcp::model::EmailAddress("Person@Example.com".to_owned());
    assert_eq!(
        store
            .add_email_member(&acme, &email)
            .await
            .expect("member")
            .0,
        "person@example.com"
    );
    assert_eq!(
        store.org_for_email(&email).await.expect("lookup"),
        Some(acme.clone())
    );
    assert_eq!(
        store.add_category(&acme, " Docs ").await.expect("category"),
        "Docs"
    );
    assert_eq!(store.categories(&acme).await.expect("list"), ["Docs"]);
    assert_eq!(
        store.set_color(&acme, Some("#abc")).await.expect("color"),
        Some("#abc".to_owned())
    );
    assert_eq!(
        store.color_map().await.expect("map").get(&acme),
        Some(&Some("#abc".to_owned()))
    );
    assert_eq!(
        store.org_names().await.expect("names"),
        std::slice::from_ref(&acme)
    );
    assert_eq!(store.list_orgs().await.expect("orgs").len(), 1);

    assert!(store.remove_category(&acme, "Docs").await.expect("remove"));
    assert!(
        store
            .remove_email_member(&acme, &email)
            .await
            .expect("remove")
    );
    assert!(
        store
            .remove_domain(&acme, "acme.example")
            .await
            .expect("remove")
    );
    assert!(store.delete_org(&acme).await.expect("delete"));
    assert!(!store.org_exists(&acme).await.expect("exists"));

    // Errors travel through the pool unchanged.
    assert_eq!(
        validation_message(store.add_domain(&acme, "x.example").await),
        "Unknown organization \"acme\"."
    );
}
