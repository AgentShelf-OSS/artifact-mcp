use super::*;
use std::collections::BTreeSet;

use rusqlite::{Connection, params};

use super::chain::{pending_receipts_root, receipt_mac};
use super::encoding::canonical;
use crate::model::{PublisherIdentity, Viewer};

use crate::persistence::migrations::{self, MigrationContext};

fn key() -> [u8; 32] {
    [7; 32]
}

fn ledger() -> Connection {
    let mut conn = Connection::open_in_memory().expect("memory db");
    crate::persistence::migrations::apply(
        &mut conn,
        &crate::persistence::migrations::MigrationContext::default(),
    )
    .expect("migrations");
    initialize_head(&conn, &key()).expect("head");
    conn
}

fn scoped(org: &str, scopes: &[&str]) -> PublisherIdentity {
    PublisherIdentity {
        client_id: "auditor".into(),
        org: org.into(),
        label: "audit".to_owned(),
        role: "reader".to_owned(),
        scopes: Some(
            scopes
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect::<BTreeSet<_>>(),
        ),
    }
}

fn append(conn: &mut Connection, tenant: &str, id: &str) {
    let tx = conn.transaction().expect("transaction");
    let mut ctx = context("mcp");
    ctx.tenant = tenant.to_owned();
    let _ = append_in_transaction(&tx, &key(), id, &ctx, &event("artifact", "a1")).expect("append");
    tx.commit().expect("commit");
}

fn context(source: &str) -> AuditContext {
    AuditContext {
        tenant: "acme".to_owned(),
        actor_type: "api_key".to_owned(),
        actor_id: "key-1".to_owned(),
        actor_role: "author".to_owned(),
        source: source.to_owned(),
        request_id: "request-1".to_owned(),
    }
}

fn event(target_type: &str, target_id: &str) -> AuditEvent {
    AuditEvent {
        operation: "artifact.publish".to_owned(),
        target_type: target_type.to_owned(),
        target_id: target_id.to_owned(),
        result: "success".to_owned(),
        classification: String::new(),
        revision: Some(1),
    }
}

fn database() -> Connection {
    let mut conn = Connection::open_in_memory().expect("in-memory database");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("apply migrations");
    conn
}

fn reserve(conn: &mut Connection, key: &[u8; 32]) {
    let tx = conn.transaction().expect("reservation transaction");
    assert!(
        reserve_receipt_in_transaction(
            &tx,
            key,
            "durability:abc",
            "publish:abc:1",
            &context("mcp"),
            &event("artifact", "abc"),
        )
        .expect("reserve authenticated receipt")
    );
    tx.commit().expect("commit receipt");
}

#[test]
fn v25_sources_and_redacted_target_types_match_node() {
    assert!(canonical("event-1", &context("worker"), &event("artifact", "a1"), "").is_err());
    assert!(
        canonical(
            "event-1",
            &context("mcp"),
            &event("public_share", "token"),
            ""
        )
        .is_err()
    );
    assert!(
        canonical(
            "event-1",
            &context("mcp"),
            &event("webhook", "wh0000000001"),
            ""
        )
        .is_ok()
    );
    assert!(
        canonical(
            "event-1",
            &context("mcp"),
            &event("webhook", "https://secret"),
            ""
        )
        .is_err()
    );
    assert!(
        canonical(
            "event-1",
            &context("mcp"),
            &event("webhook", "a-real-webhook-token"),
            ""
        )
        .is_err()
    );
    assert!(canonical("event-1", &context("mcp"), &event("artifact", "a1"), "").is_ok());
}

#[test]
fn terminal_event_ids_are_distinct_from_request_correlation() {
    let publisher = PublisherIdentity {
        client_id: "key-1".into(),
        org: "acme".into(),
        label: "test".to_owned(),
        role: "admin".to_owned(),
        scopes: None,
    };
    let audit = MutationAudit::publisher(&publisher).expect("random audit context");
    let first = audit.event_id().expect("random terminal id");
    let second = audit.event_id().expect("random terminal id");
    assert_ne!(first, second);
    assert!(
        audit
            .correlation("artifact.publish", "a1", Some(1))
            .contains(audit.context().request_id.as_str())
    );
}

#[test]
fn receipt_mac_authenticates_every_reserved_field() {
    let key = [7_u8; 32];
    let cases = [
        ("correlation_id", "durability:tampered"),
        ("durability_intent_id", "publish:other:1"),
        ("state", "finalized"),
        ("event_id", "event-other"),
        ("tenant", "other"),
        ("actor_type", "viewer"),
        ("actor_id", "other-agent"),
        ("actor_role", "admin"),
        ("source", "browser"),
        ("request_id", "request-2"),
        ("operation", "artifact.update"),
        ("target_type", "document"),
        ("target_id", "other"),
        ("result", "denied"),
        ("classification", "policy"),
        ("revision", "2"),
        ("key_id", "v2"),
        ("canonical_version", "2"),
        (
            "receipt_mac",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
    ];
    for (field, value) in cases {
        let mut conn = database();
        reserve(&mut conn, &key);
        conn.execute(
            &format!("UPDATE security_audit_receipts SET {field}=?1 WHERE correlation_id=?2"),
            params![value, "durability:abc"],
        )
        .expect("tamper receipt");
        let correlation = if field == "correlation_id" {
            value
        } else {
            "durability:abc"
        };
        let tx = conn.transaction().expect("finalization transaction");
        assert!(
            finalize_receipt_in_transaction(
                &tx,
                &key,
                correlation,
                "event-1",
                &context("mcp"),
                &event("artifact", "abc"),
            )
            .is_err(),
            "{field} tampering must fail closed"
        );
        drop(tx);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM security_audit_events", [], |row| row
                .get::<_, i64>(
                0
            ))
            .expect("event count"),
            0,
            "{field} tampering must not append an event"
        );
    }
}

#[test]
fn receipt_mac_matches_the_node_protocol_vector() {
    assert_eq!(
        receipt_mac(
            &[7_u8; 32],
            "durability:abc",
            "publish:abc:1",
            "pending",
            "",
            &context("mcp"),
            &event("artifact", "abc"),
        )
        .expect("receipt MAC"),
        "c31530275592868cd7ed2070e9f8d0da3f827905bb555283ffe6d8b2ac6af9b3"
    );
}

#[test]
fn pending_receipt_root_matches_the_node_protocol_vectors() {
    let mut conn = database();
    assert_eq!(
        pending_receipts_root(&conn).expect("empty pending root"),
        "8826f8e30ad491deb7642729c14a19fde13144b8f1b1d15e4eca84585f18be53"
    );
    reserve(&mut conn, &[7_u8; 32]);
    assert_eq!(
        pending_receipts_root(&conn).expect("one-receipt pending root"),
        "faabbef514cca7b567de7b20dc04ee845749efa0ef1cbf76b86ad784ec984b14"
    );
}

#[test]
fn reconciliation_and_compensation_reject_tampered_receipts() {
    let key = [7_u8; 32];
    let mut reconciliation = database();
    reserve(&mut reconciliation, &key);
    reconciliation
        .execute(
            "UPDATE security_audit_receipts SET tenant='other' WHERE durability_intent_id='publish:abc:1'",
            [],
        )
        .expect("tamper reconciliation receipt");
    let tx = reconciliation
        .transaction()
        .expect("reconciliation transaction");
    assert!(
        finalize_reconciled_receipt_in_transaction(
            &tx,
            &key,
            "publish:abc:1",
            "event-1",
            "recovered",
            "",
        )
        .is_err()
    );
    drop(tx);
    assert_eq!(
        reconciliation
            .query_row("SELECT COUNT(*) FROM security_audit_events", [], |row| row
                .get::<_, i64>(
                0
            ))
            .expect("event count"),
        0
    );

    let mut compensation = database();
    reserve(&mut compensation, &key);
    compensation
        .execute(
            "UPDATE security_audit_receipts SET target_id='other' WHERE durability_intent_id='publish:abc:1'",
            [],
        )
        .expect("tamper compensation receipt");
    let tx = compensation
        .transaction()
        .expect("compensation transaction");
    assert!(
        verify_pending_receipt_in_transaction(&tx, &key, "publish:abc:1").is_err(),
        "compensation must authenticate before deleting"
    );
    drop(tx);
    assert_eq!(
        compensation
            .query_row("SELECT COUNT(*) FROM security_audit_receipts", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("receipt count"),
        1
    );
}

#[test]
fn only_verified_browser_administrators_can_scope_to_another_tenant() {
    let admin = Viewer {
        email: Some("admin@example.test".into()),
        org: Some("home".into()),
        is_admin: true,
    };
    let member = Viewer {
        email: Some("member@example.test".into()),
        org: Some("home".into()),
        is_admin: false,
    };
    let scoped = MutationAudit::viewer(&admin)
        .expect("admin audit")
        .admin_for_tenant("target")
        .expect("admin can scope");
    assert_eq!(scoped.context().tenant, "target");
    assert!(
        MutationAudit::viewer(&member)
            .expect("member audit")
            .admin_for_tenant("target")
            .is_err()
    );
}

#[test]
fn audit_access_is_explicit_cursor_bound_and_cross_tenant_denied() {
    let mut conn = ledger();
    append(&mut conn, "acme", "event-1");
    append(&mut conn, "acme", "event-2");
    append(&mut conn, "beta", "event-3");
    let reader = scoped("acme", &[AUDIT_READ_CAPABILITY]);
    assert!(
        query_authorized(
            &conn,
            &key(),
            &reader,
            AuditQuery {
                tenant: None,
                cursor: None,
                limit: Some(1)
            }
        )
        .is_ok()
    );
    let page = query_authorized(
        &conn,
        &key(),
        &reader,
        AuditQuery {
            tenant: None,
            cursor: None,
            limit: Some(1),
        },
    )
    .expect("page");
    assert_eq!(page.events.len(), 1);
    assert!(page.next.is_some());
    assert!(
        query_authorized(
            &conn,
            &key(),
            &reader,
            AuditQuery {
                tenant: None,
                cursor: Some(format!("{}x", page.next.as_deref().expect("cursor"))),
                limit: None
            }
        )
        .is_err()
    );
    assert!(
        query_authorized(
            &conn,
            &key(),
            &reader,
            AuditQuery {
                tenant: Some("beta".to_owned()),
                cursor: page.next.clone(),
                limit: None
            }
        )
        .is_err()
    );
    let global = scoped("acme", &[AUDIT_READ_CAPABILITY, AUDIT_GLOBAL_CAPABILITY]);
    assert_eq!(
        query_authorized(
            &conn,
            &key(),
            &global,
            AuditQuery {
                tenant: Some("beta".to_owned()),
                cursor: None,
                limit: None
            }
        )
        .expect("global")
        .events
        .len(),
        1
    );
    assert!(
        query_authorized(
            &conn,
            &key(),
            &PublisherIdentity {
                scopes: None,
                ..reader
            },
            AuditQuery::default()
        )
        .is_err()
    );
}

#[test]
fn deleted_pending_receipt_breaks_the_head_commitment_before_legacy_bypass() {
    let key = [7_u8; 32];
    let mut conn = database();
    reserve(&mut conn, &key);
    conn.execute(
        "DELETE FROM security_audit_receipts WHERE durability_intent_id='publish:abc:1'",
        [],
    )
    .expect("delete pending receipt");
    assert!(!verify(&conn, &key).expect("verify ledger"));
    let tx = conn.transaction().expect("reconciliation transaction");
    assert!(
        finalize_reconciled_receipt_in_transaction(
            &tx,
            &key,
            "publish:abc:1",
            "event-1",
            "recovered",
            "reconciliation",
        )
        .is_err(),
        "missing authenticated receipt must not be treated as legacy"
    );
}

#[test]
fn export_is_bounded_and_retention_checkpoints_a_contiguous_prefix() {
    let mut conn = ledger();
    append(&mut conn, "acme", "old-1");
    append(&mut conn, "acme", "new");
    append(&mut conn, "acme", "old-after-new");
    let reader = scoped("acme", &[AUDIT_READ_CAPABILITY, AUDIT_EXPORT_CAPABILITY]);
    let exported = export_authorized(
        &conn,
        &key(),
        &reader,
        AuditExportQuery {
            tenant: None,
            cursor: None,
            limit: Some(10_001),
        },
    )
    .expect("export");
    assert_eq!(exported.rows, 3);
    assert!(exported.ndjson.ends_with('\n'));
    assert_eq!(
        prune_expired(&mut conn, &key(), Some("9999-01-01T00:00:00Z"), 1_000).expect("prune"),
        3
    );
    assert!(verify(&conn, &key()).expect("verify"));
    assert_eq!(query_tenant(&conn, "acme", 0, 500).expect("rows").len(), 0);
}

#[test]
fn finalized_duplicate_validates_the_referenced_event_projection() {
    let key = [7_u8; 32];
    let mut conn = database();
    reserve(&mut conn, &key);
    let tx = conn.transaction().expect("finalization transaction");
    finalize_receipt_in_transaction(
        &tx,
        &key,
        "durability:abc",
        "event-1",
        &context("mcp"),
        &event("artifact", "abc"),
    )
    .expect("finalize receipt");
    tx.commit().expect("commit finalization");
    conn.execute(
        "UPDATE security_audit_events SET target_id='other' WHERE event_id='event-1'",
        [],
    )
    .expect("tamper referenced event");
    let tx = conn.transaction().expect("duplicate transaction");
    assert!(
        finalize_receipt_in_transaction(
            &tx,
            &key,
            "durability:abc",
            "event-2",
            &context("mcp"),
            &event("artifact", "abc"),
        )
        .is_err(),
        "a receipt must not point at a mismatched terminal projection"
    );
}
