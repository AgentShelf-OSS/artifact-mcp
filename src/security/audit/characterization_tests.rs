//! Characterization of the ledger wire format, chain linkage and fail-closed paths.
//!
//! Every golden value in this module was computed on the unmodified single-file ledger
//! (origin/master 38d88c6) before the module split. A change to any of them is a protocol
//! change, not a refactor.

use super::*;
use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rusqlite::{Connection, params};

use super::chain::{checkpoint_mac, hash, head_mac, load_head, receipt_mac};
use super::encoding::{ReceiptMacInput, canonical, canonical_receipt_bytes};
use super::query::{cursor_for, cursor_from};
use crate::error::AppError;
use crate::model::PublisherIdentity;

use crate::persistence::migrations::{self, MigrationContext};

const KEY: [u8; 32] = [7; 32];
const OTHER_KEY: [u8; 32] = [9; 32];
const EMPTY_PENDING_ROOT: &str = "8826f8e30ad491deb7642729c14a19fde13144b8f1b1d15e4eca84585f18be53";
const ONE_PENDING_ROOT: &str = "faabbef514cca7b567de7b20dc04ee845749efa0ef1cbf76b86ad784ec984b14";

const GOLDEN_CANONICAL_1: &str = "01000000076576656e742d310000000276310000000461636d65000000076170695f6b6579000000056b65792d3100000006617574686f720000001061727469666163742e7075626c697368000000086172746966616374000000026131000000077375636365737300000000000000036d637000000009726571756573742d31000000013100000014323032362d30312d30315430303a30303a30305a";
const GOLDEN_EVENT_HASHES: [&str; 3] = [
    "33542b3d1fbcb9c4077300bfff6cf8bb8fbf2d561feac4d483a0c0c59086cb2e",
    "27c42abf149c9de605ba72f5ab19a43b0bb9864fce69f3700bef4a23022085e7",
    "028e61404a6ef812cf84949b1498f5a84108f2ae682b2114a8d943f07b8a1496",
];
const GOLDEN_INITIAL_HEAD_MAC: &str =
    "2e12049c357e4831127ef8587cd32a25e2333f7f4e1b19de92aaf3d920983393";
const GOLDEN_HEAD_MAC_3: &str = "fd61acd94df08f395e6d5e668e5b581e6dbe0f1432b3624a877e622bc9c7733b";
const GOLDEN_PENDING_HEAD_MAC: &str =
    "50c493666fe6cd0e550719e81502ea29cbccc9431ab58f9a0b4e164fcc0bdcb2";
const GOLDEN_CHECKPOINTS: [&str; 2] = [
    "ea9cbd59e1f845d8c04cb67ab1cbe3d8759a984f584a4ebe4452c5c4d899e9ac",
    "e06ad2176c9b85dffa2ab967dbe482536387463c6731019529a3b2de8c818bd7",
];
const GOLDEN_CURSOR: &str = "eyJ2IjoxLCJ0ZW5hbnQiOiJhY21lIiwibGFzdF9zZXF1ZW5jZSI6NX0.c43aedd380943810ff5cdf69606e38125a2987491382ab574c7108da45c0be80";
const GOLDEN_FINALIZED_RECEIPT_MAC: &str =
    "ec708f4cc1ca4ecb46defa801062cfddaa75fccacd92ec3e1d4979b8ec39d9a2";
const GOLDEN_ROW_JSON: &str = r#"{"sequence":7,"event_id":"event-7","tenant":"acme","operation":"artifact.publish","target_type":"artifact","target_id":"a1","result":"success","classification":"","occurred_at":"2026-01-01T00:00:00Z","event_hash":"abc"}"#;

fn database() -> Connection {
    let mut conn = Connection::open_in_memory().expect("in-memory database");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("apply migrations");
    conn
}

fn mcp_context() -> AuditContext {
    AuditContext {
        tenant: "acme".to_owned(),
        actor_type: "api_key".to_owned(),
        actor_id: "key-1".to_owned(),
        actor_role: "author".to_owned(),
        source: "mcp".to_owned(),
        request_id: "request-1".to_owned(),
    }
}

fn event(
    operation: &str,
    target_type: &str,
    target_id: &str,
    classification: &str,
    revision: Option<u64>,
) -> AuditEvent {
    AuditEvent {
        operation: operation.to_owned(),
        target_type: target_type.to_owned(),
        target_id: target_id.to_owned(),
        result: "success".to_owned(),
        classification: classification.to_owned(),
        revision,
    }
}

/// The fixed three-entry input sequence: one per actor type and source family.
fn chain_inputs() -> Vec<(&'static str, AuditContext, AuditEvent, &'static str)> {
    vec![
        (
            "event-1",
            mcp_context(),
            event("artifact.publish", "artifact", "a1", "", Some(1)),
            "2026-01-01T00:00:00Z",
        ),
        (
            "event-2",
            AuditContext {
                tenant: "beta".to_owned(),
                actor_type: "viewer".to_owned(),
                actor_id: "admin@example.test".to_owned(),
                actor_role: "admin".to_owned(),
                source: "browser".to_owned(),
                request_id: "browser-0001".to_owned(),
            },
            event("artifact.update", "artifact", "a1", "policy", Some(2)),
            "2026-01-02T03:04:05Z",
        ),
        (
            "event-3",
            AuditContext {
                tenant: "system".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: "artifact-mcp".to_owned(),
                actor_role: String::new(),
                source: "maintenance".to_owned(),
                request_id: "maintenance-0001".to_owned(),
            },
            event(
                "audit.prune",
                "audit_ledger",
                "retention",
                "retention",
                None,
            ),
            "2026-01-03T00:00:00.5Z",
        ),
    ]
}

/// The chain computed from the pinned primitives: `(canonical bytes, event hash)` per entry.
fn computed_chain() -> Vec<(Vec<u8>, String)> {
    let mut previous = String::new();
    let mut out = Vec::new();
    for (index, (event_id, context, event, occurred_at)) in chain_inputs().iter().enumerate() {
        let sequence = u64::try_from(index + 1).expect("small sequence");
        let bytes = canonical(event_id, context, event, occurred_at).expect("canonical");
        let event_hash = hash(&KEY, sequence, &previous, &bytes);
        previous.clone_from(&event_hash);
        out.push((bytes, event_hash));
    }
    out
}

/// A sealed ledger holding exactly the golden chain, written with the same column layout
/// `append_in_transaction` uses.
fn golden_ledger() -> Connection {
    let conn = database();
    initialize_head(&conn, &KEY).expect("seal head");
    let mut previous = String::new();
    for (index, ((event_id, context, event, occurred_at), (bytes, event_hash))) in
        chain_inputs().iter().zip(computed_chain()).enumerate()
    {
        let sequence = i64::try_from(index + 1).expect("small sequence");
        let revision = event
            .revision
            .map(|value| i64::try_from(value).expect("revision"));
        conn.execute(
            "INSERT INTO security_audit_events (sequence,event_id,key_id,tenant,actor_type,\
             actor_id,actor_role,operation,target_type,target_id,result,classification,source,\
             request_id,revision,occurred_at,canonical_version,canonical,prev_hash,event_hash) \
             VALUES (?1,?2,'v1',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,1,?16,?17,?18)",
            params![
                sequence,
                event_id,
                context.tenant,
                context.actor_type,
                context.actor_id,
                context.actor_role,
                event.operation,
                event.target_type,
                event.target_id,
                event.result,
                event.classification,
                context.source,
                context.request_id,
                revision,
                occurred_at,
                bytes,
                previous,
                event_hash
            ],
        )
        .expect("insert golden event");
        previous = event_hash;
    }
    conn.execute(
        "UPDATE security_audit_chain_head SET sequence=3,head_hash=?1,head_mac=?2 \
         WHERE singleton=1",
        params![previous, head_mac(&KEY, 3, &previous, EMPTY_PENDING_ROOT)],
    )
    .expect("advance golden head");
    conn
}

fn event_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM security_audit_events", [], |row| {
        row.get(0)
    })
    .expect("event count")
}

fn validation(message: &str) -> AppError {
    AppError::Validation(message.to_owned())
}

fn reserve(conn: &mut Connection) {
    let tx = conn.transaction().expect("reservation transaction");
    assert_eq!(
        reserve_receipt_in_transaction(
            &tx,
            &KEY,
            "durability:abc",
            "publish:abc:1",
            &mcp_context(),
            &event("artifact.publish", "artifact", "abc", "", Some(1)),
        ),
        Ok(true)
    );
    tx.commit().expect("commit receipt");
}

fn scoped(org: &str, scopes: Option<&[&str]>) -> PublisherIdentity {
    PublisherIdentity {
        client_id: "auditor".into(),
        org: org.into(),
        label: "audit".to_owned(),
        role: "reader".to_owned(),
        scopes: scopes.map(|scopes| {
            scopes
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect::<BTreeSet<_>>()
        }),
    }
}

// ---- canonical encoding and the HMAC chain ------------------------------------------

#[test]
fn golden_canonical_bytes_are_length_prefixed_and_trimmed() {
    let (event_id, context, event, occurred_at) = chain_inputs().remove(0);
    let bytes = canonical(event_id, &context, &event, occurred_at).expect("canonical");
    assert_eq!(hex::encode(&bytes), GOLDEN_CANONICAL_1);
    // Surrounding whitespace never reaches the ledger bytes.
    let mut padded_context = context.clone();
    padded_context.tenant = " acme ".to_owned();
    padded_context.request_id = "request-1\t".to_owned();
    let mut padded_event = event.clone();
    padded_event.operation = " artifact.publish".to_owned();
    padded_event.target_id = "a1 ".to_owned();
    assert_eq!(
        canonical(
            " event-1 ",
            &padded_context,
            &padded_event,
            " 2026-01-01T00:00:00Z "
        )
        .expect("padded canonical"),
        bytes
    );
}

#[test]
fn golden_event_chain_links_each_entry_to_its_predecessor() {
    let chain = computed_chain();
    let hashes: Vec<&str> = chain
        .iter()
        .map(|(_, event_hash)| event_hash.as_str())
        .collect();
    assert_eq!(hashes, GOLDEN_EVENT_HASHES);
    // The key is part of every link.
    let (bytes, _) = &chain[0];
    assert_ne!(hash(&OTHER_KEY, 1, "", bytes), GOLDEN_EVENT_HASHES[0]);
    // The sequence and the predecessor are part of every link.
    assert_ne!(hash(&KEY, 2, "", bytes), GOLDEN_EVENT_HASHES[0]);
    assert_ne!(hash(&KEY, 1, "x", bytes), GOLDEN_EVENT_HASHES[0]);
}

#[test]
fn golden_head_and_checkpoint_macs() {
    assert_eq!(
        head_mac(&KEY, 0, "", EMPTY_PENDING_ROOT),
        GOLDEN_INITIAL_HEAD_MAC
    );
    assert_eq!(
        head_mac(&KEY, 3, GOLDEN_EVENT_HASHES[2], EMPTY_PENDING_ROOT),
        GOLDEN_HEAD_MAC_3
    );
    assert_eq!(
        head_mac(&KEY, 0, "", ONE_PENDING_ROOT),
        GOLDEN_PENDING_HEAD_MAC
    );
    let first = checkpoint_mac(&KEY, 1, 1, GOLDEN_EVENT_HASHES[0], "");
    assert_eq!(first, GOLDEN_CHECKPOINTS[0]);
    assert_eq!(
        checkpoint_mac(&KEY, 2, 2, GOLDEN_EVENT_HASHES[1], &first),
        GOLDEN_CHECKPOINTS[1]
    );
}

#[test]
fn golden_finalized_receipt_mac() {
    assert_eq!(
        receipt_mac(
            &KEY,
            "durability:abc",
            "publish:abc:1",
            "finalized",
            "event-1",
            &mcp_context(),
            &event("artifact.publish", "artifact", "abc", "", Some(1)),
        ),
        Ok(GOLDEN_FINALIZED_RECEIPT_MAC.to_owned())
    );
}

#[test]
fn seal_commits_the_empty_pending_root_and_reserve_recommits_it() {
    let mut conn = database();
    initialize_head(&conn, &KEY).expect("seal head");
    let head = load_head(&conn).expect("head");
    assert_eq!(head.pending_receipts_root, EMPTY_PENDING_ROOT);
    assert_eq!(head.head_mac, GOLDEN_INITIAL_HEAD_MAC);
    // Sealing twice is a no-op, not a reseal.
    initialize_head(&conn, &KEY).expect("reseal is idempotent");
    reserve(&mut conn);
    let head = load_head(&conn).expect("head");
    assert_eq!(head.pending_receipts_root, ONE_PENDING_ROOT);
    assert_eq!(head.head_mac, GOLDEN_PENDING_HEAD_MAC);
    assert_eq!(head.sequence, 0);
}

#[test]
fn append_in_transaction_writes_the_pinned_chain_primitives() {
    let mut conn = database();
    initialize_head(&conn, &KEY).expect("seal head");
    let mut returned = Vec::new();
    for (event_id, context, event, _) in chain_inputs() {
        let tx = conn.transaction().expect("transaction");
        returned
            .push(append_in_transaction(&tx, &KEY, event_id, &context, &event).expect("append"));
        tx.commit().expect("commit");
    }
    let mut previous = String::new();
    for ((row, (event_id, context, event, _)), sequence) in
        returned.iter().zip(chain_inputs()).zip(1_u64..)
    {
        let (stored, prev, event_hash, occurred_at): (Vec<u8>, String, String, String) = conn
            .query_row(
                "SELECT canonical,prev_hash,event_hash,occurred_at FROM security_audit_events \
                 WHERE sequence=?1",
                [i64::try_from(sequence).expect("sequence")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("stored event");
        assert!(occurred_at.ends_with('Z'), "server-generated UTC RFC 3339");
        assert_eq!(
            canonical(event_id, &context, &event, &occurred_at).expect("canonical"),
            stored
        );
        assert_eq!(prev, previous);
        assert_eq!(event_hash, hash(&KEY, sequence, &prev, &stored));
        assert_eq!(
            row,
            &AuditRow {
                sequence,
                event_id: event_id.to_owned(),
                tenant: context.tenant.clone(),
                operation: event.operation.clone(),
                target_type: event.target_type.clone(),
                target_id: event.target_id.clone(),
                result: event.result.clone(),
                classification: event.classification.clone(),
                occurred_at,
                event_hash: event_hash.clone(),
            }
        );
        previous = event_hash;
    }
    let head = load_head(&conn).expect("head");
    assert_eq!(head.sequence, 3);
    assert_eq!(head.head_hash, previous);
    assert_eq!(
        head.head_mac,
        head_mac(&KEY, 3, &previous, EMPTY_PENDING_ROOT)
    );
    assert_eq!(verify(&conn, &KEY), Ok(true));
}

#[test]
fn append_fails_closed_on_a_forged_head_or_a_rejected_event() {
    let mut conn = database();
    initialize_head(&conn, &KEY).expect("seal head");
    conn.execute(
        "UPDATE security_audit_chain_head SET head_mac='00' WHERE singleton=1",
        [],
    )
    .expect("forge head");
    let tx = conn.transaction().expect("transaction");
    assert_eq!(
        append_in_transaction(
            &tx,
            &KEY,
            "event-1",
            &mcp_context(),
            &event("artifact.publish", "artifact", "a1", "", Some(1)),
        ),
        Err(AppError::Internal)
    );
    drop(tx);
    assert_eq!(event_count(&conn), 0);

    let mut conn = database();
    initialize_head(&conn, &KEY).expect("seal head");
    let tx = conn.transaction().expect("transaction");
    assert_eq!(
        append_in_transaction(
            &tx,
            &OTHER_KEY,
            "event-1",
            &mcp_context(),
            &event("artifact.publish", "artifact", "a1", "", Some(1)),
        ),
        Err(AppError::Internal),
        "a head sealed with another key must not accept appends"
    );
    assert_eq!(
        append_in_transaction(
            &tx,
            &KEY,
            "event-1",
            &mcp_context(),
            &event("artifact.share", "public_share", "token-value", "", None),
        ),
        Err(validation("audit events must not retain share tokens"))
    );
    drop(tx);
    assert_eq!(event_count(&conn), 0);
}

// ---- verification -------------------------------------------------------------------

#[test]
fn verify_accepts_exactly_the_golden_chain() {
    let conn = golden_ledger();
    assert_eq!(verify(&conn, &KEY), Ok(true));
    assert_eq!(verify(&conn, &OTHER_KEY), Ok(false));
}

#[test]
fn verify_rejects_a_tampered_entry() {
    for statement in [
        "UPDATE security_audit_events SET target_id='a2' WHERE sequence=2",
        "UPDATE security_audit_events SET classification='other' WHERE sequence=1",
        "UPDATE security_audit_events SET revision=9 WHERE sequence=2",
        "UPDATE security_audit_events SET occurred_at='2026-01-02T03:04:06Z' WHERE sequence=2",
        "UPDATE security_audit_events SET canonical=X'00' WHERE sequence=2",
        "UPDATE security_audit_events SET event_hash='00' WHERE sequence=3",
        "UPDATE security_audit_events SET prev_hash='' WHERE sequence=2",
        "UPDATE security_audit_events SET key_id='v2' WHERE sequence=1",
        "UPDATE security_audit_events SET canonical_version=2 WHERE sequence=1",
        "UPDATE security_audit_chain_head SET head_mac='00' WHERE singleton=1",
        "UPDATE security_audit_chain_head SET sequence=2 WHERE singleton=1",
        "UPDATE security_audit_chain_head SET pending_receipts_root='00' WHERE singleton=1",
    ] {
        let conn = golden_ledger();
        conn.execute(statement, []).expect("tamper");
        assert_eq!(verify(&conn, &KEY), Ok(false), "{statement}");
    }
}

#[test]
fn verify_propagates_admission_errors_for_inadmissible_tampered_fields() {
    // A tampered field the canonical encoder refuses is an error, not `Ok(false)`: the
    // encoder answers before the stored-bytes comparison.
    let conn = golden_ledger();
    conn.execute(
        "UPDATE security_audit_events SET operation='Artifact.Publish' WHERE sequence=2",
        [],
    )
    .expect("tamper");
    assert_eq!(
        verify(&conn, &KEY),
        Err(validation(
            "audit fields must be server-controlled identifiers"
        ))
    );
    let conn = golden_ledger();
    conn.execute(
        "UPDATE security_audit_events SET tenant=?1 WHERE sequence=1",
        [&"t".repeat(81)],
    )
    .expect("tamper");
    assert_eq!(
        verify(&conn, &KEY),
        Err(validation("invalid audit context"))
    );
    // An earlier entry that merely mismatches still answers first.
    let conn = golden_ledger();
    conn.execute_batch(
        "UPDATE security_audit_events SET target_id='a2' WHERE sequence=1;
         UPDATE security_audit_events SET operation='Artifact.Publish' WHERE sequence=2;",
    )
    .expect("tamper");
    assert_eq!(verify(&conn, &KEY), Ok(false));
}

#[test]
fn verify_rejects_a_reordered_entry() {
    let conn = golden_ledger();
    conn.execute_batch(
        "UPDATE security_audit_events SET sequence=-1 WHERE sequence=1;
         UPDATE security_audit_events SET sequence=1 WHERE sequence=2;
         UPDATE security_audit_events SET sequence=2 WHERE sequence=-1;",
    )
    .expect("swap entries");
    assert_eq!(verify(&conn, &KEY), Ok(false));
}

#[test]
fn verify_rejects_a_missing_entry() {
    for sequence in 1..=3 {
        let conn = golden_ledger();
        conn.execute(
            "DELETE FROM security_audit_events WHERE sequence=?1",
            [sequence],
        )
        .expect("delete entry");
        assert_eq!(
            verify(&conn, &KEY),
            Ok(false),
            "missing sequence {sequence}"
        );
    }
}

#[test]
fn verify_bridges_a_golden_checkpoint_and_rejects_a_forged_one() {
    let insert = "INSERT INTO security_audit_checkpoints (first_sequence,last_sequence,key_id,\
                  canonical_version,bridge_hash,prev_checkpoint_hash,checkpoint_hash) \
                  VALUES (?1,?2,'v1',1,?3,?4,?5)";
    let pruned = || {
        let conn = golden_ledger();
        conn.execute(
            insert,
            params![1, 1, GOLDEN_EVENT_HASHES[0], "", GOLDEN_CHECKPOINTS[0]],
        )
        .expect("insert checkpoint");
        conn.execute("DELETE FROM security_audit_events WHERE sequence=1", [])
            .expect("prune entry");
        conn
    };
    let conn = pruned();
    assert_eq!(verify(&conn, &KEY), Ok(true));
    conn.execute(
        insert,
        params![
            2,
            2,
            GOLDEN_EVENT_HASHES[1],
            GOLDEN_CHECKPOINTS[0],
            GOLDEN_CHECKPOINTS[1]
        ],
    )
    .expect("insert second checkpoint");
    conn.execute("DELETE FROM security_audit_events WHERE sequence=2", [])
        .expect("prune entry");
    assert_eq!(verify(&conn, &KEY), Ok(true));
    for statement in [
        "UPDATE security_audit_checkpoints SET checkpoint_hash='00'",
        "UPDATE security_audit_checkpoints SET bridge_hash='00'",
        "UPDATE security_audit_checkpoints SET first_sequence=0",
        "UPDATE security_audit_checkpoints SET key_id='v2'",
        "UPDATE security_audit_checkpoints SET prev_checkpoint_hash='00'",
        "DELETE FROM security_audit_checkpoints",
    ] {
        let conn = pruned();
        conn.execute(statement, []).expect("tamper checkpoint");
        assert_eq!(verify(&conn, &KEY), Ok(false), "{statement}");
    }
}

#[test]
fn verify_rejects_a_tampered_bridge_that_only_the_checkpoint_mac_covers() {
    // With two checkpoints, the events stage links to the second checkpoint's bridge, so the
    // first checkpoint's bridge is protected by its MAC alone.
    let insert = "INSERT INTO security_audit_checkpoints (first_sequence,last_sequence,key_id,\
                  canonical_version,bridge_hash,prev_checkpoint_hash,checkpoint_hash) \
                  VALUES (?1,?2,'v1',1,?3,?4,?5)";
    let conn = golden_ledger();
    conn.execute(
        insert,
        params![1, 1, GOLDEN_EVENT_HASHES[0], "", GOLDEN_CHECKPOINTS[0]],
    )
    .expect("insert first checkpoint");
    conn.execute(
        insert,
        params![
            2,
            2,
            GOLDEN_EVENT_HASHES[1],
            GOLDEN_CHECKPOINTS[0],
            GOLDEN_CHECKPOINTS[1]
        ],
    )
    .expect("insert second checkpoint");
    conn.execute("DELETE FROM security_audit_events WHERE sequence<=2", [])
        .expect("prune entries");
    assert_eq!(verify(&conn, &KEY), Ok(true));
    conn.execute(
        "UPDATE security_audit_checkpoints SET bridge_hash='00' WHERE first_sequence=1",
        [],
    )
    .expect("tamper first bridge");
    assert_eq!(verify(&conn, &KEY), Ok(false));
}

#[test]
fn retention_refuses_to_prune_a_tampered_ledger() {
    let mut conn = golden_ledger();
    conn.execute(
        "UPDATE security_audit_events SET target_id='a2' WHERE sequence=2",
        [],
    )
    .expect("tamper");
    assert_eq!(
        prune_expired(&mut conn, &KEY, Some("9999-01-01T00:00:00Z"), 1_000),
        Err(validation(
            "audit ledger integrity verification failed before retention"
        ))
    );
    assert_eq!(event_count(&conn), 3);
    let mut conn = golden_ledger();
    assert_eq!(
        prune_expired(&mut conn, &KEY, Some("not a date"), 1_000),
        Err(validation("invalid audit retention cutoff"))
    );
    assert_eq!(
        prune_expired(&mut conn, &KEY, Some("2026-01-02T00:00:00Z"), 1_000),
        Ok(1)
    );
    assert_eq!(verify(&conn, &KEY), Ok(true));
    let checkpoint: (i64, i64, String, String, String) = conn
        .query_row(
            "SELECT first_sequence,last_sequence,bridge_hash,prev_checkpoint_hash,\
             checkpoint_hash FROM security_audit_checkpoints",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("checkpoint");
    assert_eq!(
        checkpoint,
        (
            1,
            1,
            GOLDEN_EVENT_HASHES[0].to_owned(),
            String::new(),
            GOLDEN_CHECKPOINTS[0].to_owned()
        )
    );
}

// ---- receipts -----------------------------------------------------------------------

#[test]
fn receipt_transitions_answer_with_their_exact_errors() {
    let receipt_event = || event("artifact.publish", "artifact", "abc", "", Some(1));
    let mut conn = database();
    reserve(&mut conn);
    let tx = conn.transaction().expect("transaction");
    assert_eq!(
        finalize_receipt_in_transaction(
            &tx,
            &KEY,
            "durability:missing",
            "event-1",
            &mcp_context(),
            &receipt_event(),
        ),
        Err(AppError::Internal)
    );
    let mut other = mcp_context();
    other.actor_id = "key-2".to_owned();
    assert_eq!(
        finalize_receipt_in_transaction(
            &tx,
            &KEY,
            "durability:abc",
            "event-1",
            &other,
            &receipt_event(),
        ),
        Err(validation("audit receipt context mismatch"))
    );
    assert_eq!(
        finalize_reconciled_receipt_in_transaction(
            &tx,
            &KEY,
            "publish:legacy:1",
            "event-1",
            "recovered",
            ""
        ),
        Ok(false),
        "legacy intents without a receipt do not block recovery"
    );
    assert_eq!(
        finalize_reconciled_receipt_in_transaction(
            &tx,
            &KEY,
            "publish:abc:1",
            "event-1",
            "success",
            ""
        ),
        Err(AppError::Internal)
    );
    assert_eq!(
        reserve_receipt_in_transaction(
            &tx,
            &KEY,
            "durability:retry",
            "publish:abc:1",
            &other,
            &receipt_event(),
        ),
        Ok(false),
        "a retry never rewrites a pending reservation"
    );
    let (first, duplicate) = finalize_receipt_in_transaction(
        &tx,
        &KEY,
        "durability:abc",
        "event-1",
        &mcp_context(),
        &receipt_event(),
    )
    .expect("finalize");
    assert!(!duplicate);
    assert_eq!(
        finalize_receipt_in_transaction(
            &tx,
            &KEY,
            "durability:abc",
            "event-2",
            &mcp_context(),
            &receipt_event(),
        ),
        Ok((first, true))
    );
    let stored_mac: String = tx
        .query_row(
            "SELECT receipt_mac FROM security_audit_receipts WHERE correlation_id='durability:abc'",
            [],
            |row| row.get(0),
        )
        .expect("receipt mac");
    assert_eq!(stored_mac, GOLDEN_FINALIZED_RECEIPT_MAC);
    drop(tx);
}

#[test]
fn a_tampered_receipt_answers_before_a_context_mismatch() {
    let mut conn = database();
    reserve(&mut conn);
    conn.execute(
        "UPDATE security_audit_receipts SET target_id='other' WHERE correlation_id='durability:abc'",
        [],
    )
    .expect("tamper receipt");
    let tx = conn.transaction().expect("transaction");
    let mut other = mcp_context();
    other.actor_id = "key-2".to_owned();
    // The pending-root commitment still matches (it covers receipt MACs only), but the
    // pending-set check authenticates every snapshot before the context comparison.
    assert_eq!(
        finalize_receipt_in_transaction(
            &tx,
            &KEY,
            "durability:abc",
            "event-1",
            &other,
            &event("artifact.publish", "artifact", "abc", "", Some(1)),
        ),
        Err(AppError::Internal)
    );
    assert_eq!(
        verify_pending_receipt_in_transaction(&tx, &KEY, "publish:abc:1"),
        Err(AppError::Internal)
    );
    assert_eq!(
        delete_pending_receipt_in_transaction(&tx, &KEY, "publish:abc:1"),
        Err(AppError::Internal)
    );
    // Count inside the transaction: dropping it rolls back, so a count after the drop would be
    // zero even if the failed finalize had appended an event.
    assert_eq!(event_count(&tx), 0);
    drop(tx);
}

#[test]
fn receipt_snapshot_admission_rules() {
    let context = mcp_context();
    let event = event("artifact.publish", "artifact", "abc", "", Some(1));
    let input = |correlation_id: &'static str, state: &'static str, event_id: &'static str| {
        receipt_input(&context, &event, correlation_id, state, event_id)
    };
    assert!(canonical_receipt_bytes(input("durability:abc", "pending", "")).is_ok());
    assert_eq!(
        canonical_receipt_bytes(input("", "pending", "")),
        Err(validation("invalid audit receipt identifier"))
    );
    let long = "c".repeat(241);
    assert_eq!(
        canonical_receipt_bytes(receipt_input(&context, &event, &long, "pending", "")),
        Err(validation("invalid audit receipt identifier"))
    );
    assert_eq!(
        canonical_receipt_bytes(input("durability:abc", "pending", "event-1")),
        Err(validation("invalid audit receipt state"))
    );
    assert_eq!(
        canonical_receipt_bytes(input("durability:abc", "finalized", "")),
        Err(validation("invalid audit receipt state"))
    );
    assert_eq!(
        canonical_receipt_bytes(input("durability:abc", "ambiguous", "")),
        Err(validation("invalid audit receipt state"))
    );
    assert_eq!(
        canonical_receipt_bytes(input("durability:abc", "finalized", "Event-1")),
        Err(validation(
            "audit fields must be server-controlled identifiers"
        ))
    );
    // Event admission answers before the receipt identifier check.
    let mut worker = mcp_context();
    worker.source = "worker".to_owned();
    assert_eq!(
        canonical_receipt_bytes(ReceiptMacInput {
            context: &worker,
            ..input("", "pending", "")
        }),
        Err(validation("invalid server-derived audit context or result"))
    );
}

fn receipt_input<'a>(
    context: &'a AuditContext,
    event: &'a AuditEvent,
    correlation_id: &'a str,
    state: &'a str,
    event_id: &'a str,
) -> ReceiptMacInput<'a> {
    ReceiptMacInput {
        correlation_id,
        durability_intent_id: "publish:abc:1",
        state,
        event_id,
        context,
        event,
        key_id: KEY_ID,
        canonical_version: 1,
    }
}

// ---- redaction and field admission --------------------------------------------------

#[test]
fn canonical_admission_rejects_with_exact_errors_in_order() {
    let check = |context: &AuditContext, event: &AuditEvent| {
        canonical("event-1", context, event, "2026-01-01T00:00:00Z").map(|_| ())
    };
    let base = || event("artifact.publish", "artifact", "a1", "", Some(1));
    let context_error: Result<(), AppError> =
        Err(validation("invalid server-derived audit context or result"));
    let identifier_error: Result<(), AppError> = Err(validation(
        "audit fields must be server-controlled identifiers",
    ));
    let bounded_error: Result<(), AppError> = Err(validation("invalid audit context"));

    for (field, value) in [("actor_type", "service"), ("source", "worker")] {
        let mut context = mcp_context();
        match field {
            "actor_type" => context.actor_type = value.to_owned(),
            _ => context.source = value.to_owned(),
        }
        assert_eq!(check(&context, &base()), context_error, "{field}");
    }
    let mut failed = base();
    failed.result = "ok".to_owned();
    assert_eq!(check(&mcp_context(), &failed), context_error);

    // Share tokens never reach the ledger, whatever the target type's case.
    for target_type in ["public_share", "PUBLIC_SHARE", "share"] {
        assert_eq!(
            check(
                &mcp_context(),
                &event("share.create", target_type, "tok", "", None)
            ),
            Err(validation("audit events must not retain share tokens")),
            "{target_type}"
        );
    }
    assert_eq!(
        check(
            &mcp_context(),
            &event("share.revoke", "public_share", "  ", "", None)
        ),
        Ok(())
    );
    // Webhook targets keep only a validated webhook id.
    assert_eq!(
        check(
            &mcp_context(),
            &event("webhook.test", "webhook", "wh0000000001", "", None)
        ),
        Ok(())
    );
    for target_id in ["https://hooks.example/secret", "a-real-webhook-token"] {
        assert_eq!(
            check(
                &mcp_context(),
                &event("webhook.test", "webhook", target_id, "", None)
            ),
            Err(validation(
                "audit events must not retain unvalidated webhook identifiers"
            )),
            "{target_id}"
        );
    }
    // Identifier fields: lowercase ASCII, digits and `._-` only, at most 100 bytes.
    for (operation, target_type, target_id, classification) in [
        ("Artifact.publish", "artifact", "a1", ""),
        ("", "artifact", "a1", ""),
        ("artifact.publish", "", "a1", ""),
        ("artifact.publish", "artifact", "a/1", ""),
        ("artifact.publish", "artifact", "a1", "has space"),
        ("artifact.publish", "artifact", &"a".repeat(101), ""),
    ] {
        assert_eq!(
            check(
                &mcp_context(),
                &event(operation, target_type, target_id, classification, None)
            ),
            identifier_error,
            "{operation} {target_type} {target_id} {classification}"
        );
    }
    assert_eq!(
        check(
            &mcp_context(),
            &event("artifact.publish", "artifact", &"a".repeat(100), "", None)
        ),
        Ok(())
    );
    // Bounded free-text context fields.
    for (field, limit) in [
        ("tenant", 80),
        ("actor_id", 160),
        ("actor_role", 40),
        ("request_id", 120),
    ] {
        let set = |context: &mut AuditContext, value: String| match field {
            "tenant" => context.tenant = value,
            "actor_id" => context.actor_id = value,
            "actor_role" => context.actor_role = value,
            _ => context.request_id = value,
        };
        let mut at_limit = mcp_context();
        set(&mut at_limit, "x".repeat(limit));
        assert_eq!(check(&at_limit, &base()), Ok(()), "{field} at limit");
        let mut over = mcp_context();
        set(&mut over, "x".repeat(limit + 1));
        assert_eq!(check(&over, &base()), bounded_error, "{field} over limit");
        let mut nul = mcp_context();
        set(&mut nul, "x\0y".to_owned());
        assert_eq!(check(&nul, &base()), bounded_error, "{field} NUL");
    }
    // Order: context/result, then share, then webhook, then field syntax.
    let mut worker = mcp_context();
    worker.source = "worker".to_owned();
    assert_eq!(
        check(&worker, &event("Share", "public_share", "tok", "", None)),
        context_error
    );
    assert_eq!(
        check(
            &mcp_context(),
            &event("Share", "public_share", "tok", "", None)
        ),
        Err(validation("audit events must not retain share tokens"))
    );
    let mut long_tenant = mcp_context();
    long_tenant.tenant = "x".repeat(81);
    // Field checks run in field order: event id, key id, tenant, ..., operation.
    assert_eq!(
        check(&long_tenant, &event("Bad", "artifact", "a1", "", None)),
        bounded_error
    );
    assert_eq!(
        canonical("Event-1", &long_tenant, &base(), "").map(|_| ()),
        identifier_error
    );
    assert_eq!(check(&long_tenant, &base()), bounded_error);
}

#[test]
fn readers_project_only_the_safe_row_fields() {
    let row = AuditRow {
        sequence: 7,
        event_id: "event-7".to_owned(),
        tenant: "acme".to_owned(),
        operation: "artifact.publish".to_owned(),
        target_type: "artifact".to_owned(),
        target_id: "a1".to_owned(),
        result: "success".to_owned(),
        classification: String::new(),
        occurred_at: "2026-01-01T00:00:00Z".to_owned(),
        event_hash: "abc".to_owned(),
    };
    assert_eq!(
        serde_json::to_string(&row).expect("row json"),
        GOLDEN_ROW_JSON
    );

    let conn = golden_ledger();
    let reader = scoped(
        "beta",
        Some(&[AUDIT_READ_CAPABILITY, AUDIT_EXPORT_CAPABILITY]),
    );
    let exported = export_authorized(
        &conn,
        &KEY,
        &reader,
        AuditExportQuery {
            tenant: None,
            cursor: None,
            limit: None,
        },
    )
    .expect("export");
    let rows = query_tenant(&conn, "beta", 0, 500).expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        exported.ndjson,
        format!("{}\n", serde_json::to_string(&rows[0]).expect("row json"))
    );
    assert!(!exported.ndjson.contains("admin@example.test"));
    assert!(!exported.ndjson.contains("browser-0001"));
    assert_eq!(rows[0].event_hash, GOLDEN_EVENT_HASHES[1]);
}

#[test]
fn audit_access_debug_and_key_parsing_never_echo_key_material() {
    let pool = r2d2::Pool::builder()
        .max_size(1)
        .build_unchecked(r2d2_sqlite::SqliteConnectionManager::memory());
    let access = AuditAccess::new(pool, KEY);
    let encoded = STANDARD.encode(KEY);
    assert_eq!(format!("{access:?}"), "AuditAccess(<redacted>)");
    assert_eq!(format!("{access:#?}"), "AuditAccess(<redacted>)");

    assert_eq!(parse_hmac_key(&encoded), Ok(KEY));
    assert_eq!(parse_hmac_key(&format!(" {encoded}\n")), Ok(KEY));
    let message = "AUDIT_LEDGER_HMAC_KEY must be canonical base64 encoding of exactly 32 bytes.";
    for value in [
        String::new(),
        "not base64!".to_owned(),
        STANDARD.encode([7_u8; 31]),
        STANDARD.encode([7_u8; 33]),
        URL_SAFE_NO_PAD.encode([0xfb_u8; 32]),
    ] {
        // The error equals a fixed message, so it cannot carry the rejected input.
        let error = parse_hmac_key(&value).expect_err("reject");
        assert_eq!(error, validation(message), "{value}");
    }
}

// ---- query admission and cursors ----------------------------------------------------

#[test]
fn golden_cursor_is_tenant_bound_and_authenticated() {
    let cursor = cursor_for(&KEY, "acme", 5).expect("cursor");
    assert_eq!(cursor, GOLDEN_CURSOR);
    assert_eq!(cursor_from(&KEY, Some(&cursor), "acme"), Ok(5));
    assert_eq!(cursor_from(&KEY, None, "acme"), Ok(0));
    assert_eq!(cursor_from(&KEY, Some(""), "acme"), Ok(0));
    assert_eq!(
        cursor_from(&KEY, Some(&cursor), "beta"),
        Err(validation("audit cursor does not match tenant"))
    );
    for forged in [
        format!("{cursor}x"),
        format!("{cursor}.extra"),
        "no-signature".to_owned(),
        cursor_for(&OTHER_KEY, "acme", 5).expect("cursor"),
    ] {
        assert_eq!(
            cursor_from(&KEY, Some(&forged), "acme"),
            Err(validation("invalid audit cursor")),
            "{forged}"
        );
    }
}

#[test]
fn query_admission_checks_tenant_then_capability_then_cursor() {
    let conn = golden_ledger();
    let query = |tenant: Option<&str>, cursor: Option<&str>| AuditQuery {
        tenant: tenant.map(str::to_owned),
        cursor: cursor.map(str::to_owned),
        limit: None,
    };
    let unscoped = scoped("acme", None);
    let reader = scoped("acme", Some(&[AUDIT_READ_CAPABILITY]));
    assert_eq!(
        query_authorized(&conn, &KEY, &unscoped, query(Some("  "), Some("bad"))),
        Err(validation("invalid audit tenant"))
    );
    assert_eq!(
        query_authorized(&conn, &KEY, &unscoped, query(None, Some("bad"))),
        Err(AppError::Forbidden(
            "audit capability is required".to_owned()
        ))
    );
    assert_eq!(
        query_authorized(&conn, &KEY, &scoped("acme", Some(&[])), query(None, None)),
        Err(AppError::Forbidden(
            "audit capability is required".to_owned()
        ))
    );
    assert_eq!(
        query_authorized(&conn, &KEY, &reader, query(Some("beta"), Some("bad"))),
        Err(AppError::Forbidden(
            "global audit capability is required".to_owned()
        ))
    );
    assert_eq!(
        query_authorized(&conn, &KEY, &reader, query(None, Some("bad"))),
        Err(validation("invalid audit cursor"))
    );
    assert_eq!(
        export_authorized(
            &conn,
            &KEY,
            &reader,
            AuditExportQuery {
                tenant: None,
                cursor: None,
                limit: None
            }
        ),
        Err(AppError::Forbidden(
            "audit capability is required".to_owned()
        ))
    );
    let page = query_authorized(&conn, &KEY, &reader, query(None, None)).expect("page");
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].event_hash, GOLDEN_EVENT_HASHES[0]);
    assert_eq!(page.next, None);
}

#[test]
fn verify_mismatch_in_an_earlier_stage_answers_before_a_later_stage_error() {
    // A forged checkpoint answers `Ok(false)` before the event stage can reject an
    // inadmissible field.
    let conn = golden_ledger();
    conn.execute_batch(
        "INSERT INTO security_audit_checkpoints (first_sequence,last_sequence,key_id,\
         canonical_version,bridge_hash,prev_checkpoint_hash,checkpoint_hash) \
         VALUES (1,1,'v1',1,'','','00');
         UPDATE security_audit_events SET operation='Artifact.Publish' WHERE sequence=2;",
    )
    .expect("tamper");
    assert_eq!(verify(&conn, &KEY), Ok(false));
    // A missing head row alone is an error.
    let conn = golden_ledger();
    conn.execute("DELETE FROM security_audit_chain_head", [])
        .expect("delete head");
    assert_eq!(verify(&conn, &KEY), Err(AppError::Internal));
    // An event mismatch answers `Ok(false)` before the head stage reads the missing head.
    let conn = golden_ledger();
    conn.execute_batch(
        "UPDATE security_audit_events SET target_id='a2' WHERE sequence=1;
         DELETE FROM security_audit_chain_head;",
    )
    .expect("tamper");
    assert_eq!(verify(&conn, &KEY), Ok(false));
}

#[test]
fn verify_rejects_a_checkpoint_from_another_canonical_version() {
    // The golden checkpoint MAC does not cover the version, so only the version check
    // rejects this row.
    let conn = golden_ledger();
    conn.execute(
        "INSERT INTO security_audit_checkpoints (first_sequence,last_sequence,key_id,\
         canonical_version,bridge_hash,prev_checkpoint_hash,checkpoint_hash) \
         VALUES (1,1,'v1',2,?1,'',?2)",
        params![GOLDEN_EVENT_HASHES[0], GOLDEN_CHECKPOINTS[0]],
    )
    .expect("insert checkpoint");
    conn.execute("DELETE FROM security_audit_events WHERE sequence=1", [])
        .expect("prune entry");
    assert_eq!(verify(&conn, &KEY), Ok(false));
}

#[test]
fn schema_refuses_a_checkpoint_with_an_inverted_range() {
    // `verify` also rejects `last < first`; the schema keeps such a row from being stored.
    let conn = golden_ledger();
    let inserted = conn.execute(
        "INSERT INTO security_audit_checkpoints (first_sequence,last_sequence,key_id,\
         canonical_version,bridge_hash,prev_checkpoint_hash,checkpoint_hash) \
         VALUES (1,0,'v1',1,?1,'',?2)",
        params![GOLDEN_EVENT_HASHES[0], GOLDEN_CHECKPOINTS[0]],
    );
    assert!(
        matches!(
            &inserted,
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ConstraintViolation,
                    ..
                },
                Some(message)
            )) if message == "CHECK constraint failed: first_sequence <= last_sequence"
        ),
        "{inserted:?}"
    );
}
