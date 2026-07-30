//! PBI-056 durable storage contract, including intent gates and restart-safe leases.
use artifact_mcp::error::AppError;
use artifact_mcp::persistence::migrations::{self, MigrationContext};
use artifact_mcp::persistence::outbox::{
    self, DeadLetterTransition, EnqueueDelivery, LEASE_MILLIS, MAX_ATTEMPTS, MAX_PAYLOAD_BYTES,
    RetryTransition,
};
use rusqlite::{Connection, TransactionBehavior};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT_TEMP_DB: AtomicU64 = AtomicU64::new(0);

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().expect("db");
    conn.execute_batch("PRAGMA foreign_keys=ON").expect("fk");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("migrate");
    conn
}

fn temporary_db_path() -> PathBuf {
    let sequence = NEXT_TEMP_DB.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "artifact-mcp-u56-empty-claim-{}-{sequence}.db",
        std::process::id()
    ))
}
fn event(name: &str, target: &str) -> EnqueueDelivery {
    EnqueueDelivery {
        event_id: name.into(),
        tenant: "acme".into(),
        event_type: "published".into(),
        target_key: target.to_owned(),
        secret_ref: "webhook:wh-safe-ref".into(),
        payload: br#"{"content":"hello"}"#.to_vec(),
        payload_sha256: None,
        durability_intent_id: None,
        delivery_kind: outbox::DELIVERY_KIND_EVENT.into(),
        ordering_key: target.to_owned(),
        depends_on_outbox_id: None,
    }
}
fn enqueue(
    conn: &mut Connection,
    input: EnqueueDelivery,
    id: &str,
    now: i64,
) -> outbox::DeliveryRecord {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("tx");
    let row = outbox::enqueue_in_transaction(&tx, &input, id, now).expect("enqueue");
    tx.commit().expect("commit");
    row
}

#[test]
fn v27_schema_is_intent_gated_bounded_and_has_required_indexes() {
    let mut conn = db();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='provider_delivery_outbox'",
            [],
            |r| r.get(0),
        )
        .expect("sql");
    assert!(sql.contains("length(payload) <= 32768"));
    assert!(sql.contains("ON DELETE RESTRICT"));
    assert!(sql.contains("state = 'blocked'"));
    assert!(!sql.contains("org_webhooks"));
    let names: Vec<String> = {
        let mut s=conn.prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='provider_delivery_outbox'").expect("i");
        s.query_map([], |r| r.get(0))
            .expect("q")
            .collect::<rusqlite::Result<_>>()
            .expect("rows")
    };
    for name in [
        "provider_delivery_outbox_ready_idx",
        "provider_delivery_outbox_tenant_idx",
        "provider_delivery_outbox_target_idx",
        "provider_delivery_outbox_intent_idx",
        "provider_delivery_outbox_bucket_idx",
    ] {
        assert!(names.contains(&name.to_owned()));
    }
    let row = enqueue(
        &mut conn,
        event("lease-check", "lease-target"),
        "lease-check",
        0,
    );
    let error = conn
        .execute(
            "UPDATE provider_delivery_outbox SET lease_owner='orphan' WHERE id=?1",
            [&row.id],
        )
        .expect_err("non-leased rows must reject partial lease state");
    assert!(error.to_string().contains("CHECK constraint failed"));
}

#[test]
fn empty_claim_does_not_require_the_writer_slot() {
    let path = temporary_db_path();
    let mut writer = Connection::open(&path).expect("writer db");
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON")
        .expect("wal");
    migrations::apply(&mut writer, &MigrationContext::empty()).expect("migrate");

    // Keep the writer slot occupied: an empty worker poll must remain read-only and therefore
    // return immediately instead of trying `BEGIN IMMEDIATE`.
    let hold = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("hold writer slot");
    let mut worker = Connection::open(&path).expect("worker db");
    worker
        .busy_timeout(Duration::from_millis(50))
        .expect("short test timeout");

    assert_eq!(
        outbox::claim_next(&mut worker, "worker", "lease", 1).expect("empty claim"),
        None
    );

    hold.commit().expect("release writer slot");
    drop(worker);
    drop(writer);
    for suffix in ["", "-wal", "-shm"] {
        let _ignored = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}
#[test]
fn enqueue_is_idempotent_payload_hashed_and_capacity_safe() {
    let mut conn = db();
    let one = enqueue(&mut conn, event("one", "a"), "one", 10);
    let same = enqueue(&mut conn, event("one", "a"), "other", 11);
    assert_eq!(one.id, same.id);
    assert!(outbox::verify_payload_hash(&one));
    assert_eq!(one.secret_ref, "webhook:wh-safe-ref");
    let mut bad = event("bad", "b");
    bad.payload_sha256 = Some("0".repeat(64));
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox::enqueue_in_transaction(&tx, &bad, "bad", 0).expect_err("hash"),
        AppError::Validation("payload hash does not match payload".into())
    );
    drop(tx);
    let too_big = EnqueueDelivery {
        payload: vec![0; MAX_PAYLOAD_BYTES + 1],
        ..event("big", "c")
    };
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox::enqueue_in_transaction(&tx, &too_big, "big", 0).expect_err("limit"),
        AppError::PayloadTooLarge
    );
}
#[test]
fn blocked_intent_never_claims_and_release_or_compensation_is_ordered() {
    let mut conn = db();
    conn.execute("INSERT INTO artifact_durability_intents (id,artifact_id,operation,state) VALUES ('intent-1','missing','publish','prepared')",[]).expect("intent");
    let mut blocked = event("blocked", "a");
    blocked.durability_intent_id = Some("intent-1".into());
    enqueue(&mut conn, blocked, "blocked", 10);
    assert_eq!(
        outbox::claim_next(&mut conn, "worker", "lease", 20).expect("claim"),
        None
    );
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox::finalize_durability_success_in_transaction(&tx, "intent-1", 30).expect("release"),
        1
    );
    tx.commit().expect("commit");
    let lease = outbox::claim_next(&mut conn, "worker", "lease", 31)
        .expect("claim")
        .expect("ready");
    assert_eq!(lease.state, "leased");
    conn.execute("INSERT INTO artifact_durability_intents (id,artifact_id,operation,state) VALUES ('intent-2','missing','publish','prepared')",[]).expect("intent");
    let mut second = event("blocked-2", "b");
    second.durability_intent_id = Some("intent-2".into());
    enqueue(&mut conn, second, "blocked-2", 40);
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox::compensate_durability_in_transaction(&tx, "intent-2").expect("compensate"),
        1
    );
    tx.commit().expect("commit");
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM artifact_durability_intents WHERE id='intent-2'",
            [],
            |r| r.get(0)
        )
        .expect("gone"),
        0
    );
}
#[test]
fn fifo_target_serialization_and_expired_lease_are_retryable_with_token_guards() {
    let mut conn = db();
    enqueue(&mut conn, event("one", "a"), "one", 10);
    enqueue(&mut conn, event("two", "a"), "two", 11);
    enqueue(&mut conn, event("three", "b"), "three", 12);
    let first = outbox::claim_next(&mut conn, "w1", "token-1", 20)
        .expect("claim")
        .expect("first");
    let other = outbox::claim_next(&mut conn, "w2", "token-2", 21)
        .expect("claim")
        .expect("other");
    assert_eq!(first.id, "one");
    assert_eq!(other.id, "three");
    assert_eq!(
        outbox::claim_next(&mut conn, "w3", "token-3", 22).expect("claim"),
        None
    );
    let next = outbox::claim_next(&mut conn, "w4", "token-4", 20 + LEASE_MILLIS + 1)
        .expect("restart")
        .expect("retry");
    assert_eq!(next.id, "one");
    assert!(next.duplicate_risk);
    assert_eq!(next.result_classification, "ambiguous_worker_restart");
    assert!(!outbox::accepted(&conn, "one", "w1", "token-1", 1, "discord", 40).expect("guard"));
    assert!(
        outbox::retry(
            &conn,
            "one",
            "w4",
            "token-4",
            next.lease_version,
            RetryTransition {
                next_attempt_at: 999,
                classification: "network_retry".into(),
                duplicate_risk: true
            },
            50
        )
        .expect("retry")
    );
}

#[test]
fn rate_limit_and_bucket_discovery_are_atomic_and_late_results_remain_finalizable() {
    let mut conn = db();
    enqueue(&mut conn, event("one", "a"), "one", 10);
    outbox::persist_rate_limit(
        &mut conn,
        "bucket",
        "a",
        "discord-bucket",
        "webhook:wh-safe-ref",
        99,
        20,
    )
    .expect("bucket");
    outbox::persist_rate_limit(
        &mut conn,
        "bucket",
        "a",
        "discord-bucket",
        "webhook:wh-safe-ref",
        50,
        21,
    )
    .expect("older response");
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT blocked_until FROM provider_delivery_rate_limits",
            [],
            |row| row.get(0)
        )
        .expect("monotonic"),
        99
    );
    assert_eq!(
        outbox::claim_next(&mut conn, "w", "t", 98).expect("blocked"),
        None
    );
    let leased = outbox::claim_next(&mut conn, "w", "t", 100)
        .expect("claim")
        .expect("row");
    assert_eq!(leased.bucket_id, "discord-bucket");
    assert!(
        outbox::accepted(
            &conn,
            &leased.id,
            "w",
            "t",
            leased.lease_version,
            "message",
            100 + LEASE_MILLIS + 1
        )
        .expect("late accepted")
    );
    assert!(
        !outbox::accepted(
            &conn,
            &leased.id,
            "w",
            "t",
            leased.lease_version,
            "message",
            100 + LEASE_MILLIS + 2
        )
        .expect("stale token")
    );
}

#[test]
fn rate_limit_keys_are_normalized_bounded_and_never_persist_raw_secrets() {
    let mut conn = db();
    let secret = "https://discord.com/api/webhooks/1/ULTRA_SECRET_TOKEN";
    let oversized_bucket = "x".repeat(129);
    for (scope, target, bucket, secret_ref) in [
        ("global", secret, "", ""),
        ("target", secret, "", ""),
        ("bucket", "wh-a", secret, "webhook:wh-safe-ref"),
        ("bucket", "wh-a", "bucket", secret),
        (
            "bucket",
            "wh-a",
            oversized_bucket.as_str(),
            "webhook:wh-safe-ref",
        ),
    ] {
        let error =
            outbox::persist_rate_limit(&mut conn, scope, target, bucket, secret_ref, 500, 100)
                .expect_err("unsafe rate-limit identity");
        assert_eq!(
            error,
            AppError::Validation("invalid rate limit state".into())
        );
    }
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM provider_delivery_rate_limits",
            [],
            |row| row.get(0)
        )
        .expect("count"),
        0
    );
    outbox::persist_rate_limit(&mut conn, "global", "", "", "", 500, 100).expect("global");
    outbox::persist_rate_limit(&mut conn, "target", "wh-a", "", "", 500, 100).expect("target");
    outbox::persist_rate_limit(
        &mut conn,
        "bucket",
        "wh-a",
        "bucket-a",
        "webhook:wh-safe-ref",
        500,
        100,
    )
    .expect("bucket");
    let rows: Vec<(String, String, String, String)> = {
        let mut statement = conn
            .prepare(
                "SELECT scope,target_key,bucket_id,top_level_secret_ref
                 FROM provider_delivery_rate_limits ORDER BY scope",
            )
            .expect("prepare");
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows")
    };
    assert_eq!(
        rows,
        vec![
            (
                "bucket".into(),
                String::new(),
                "bucket-a".into(),
                "webhook:wh-safe-ref".into()
            ),
            ("global".into(), String::new(), String::new(), String::new()),
            ("target".into(), "wh-a".into(), String::new(), String::new())
        ]
    );
    let persisted: String = conn
        .query_row(
            "SELECT group_concat(target_key || bucket_id || top_level_secret_ref, '')
             FROM provider_delivery_rate_limits",
            [],
            |row| row.get(0),
        )
        .expect("persisted rate-limit keys");
    assert!(!persisted.contains("ULTRA_SECRET_TOKEN"));
}

#[test]
fn terminal_error_codes_never_persist_a_secret_like_response() {
    let mut conn = db();
    enqueue(&mut conn, event("one", "a"), "one", 10);
    let leased = outbox::claim_next(&mut conn, "worker", "token", 20)
        .expect("claim")
        .expect("row");
    assert!(
        outbox::dead_letter(
            &conn,
            &leased.id,
            "worker",
            "token",
            leased.lease_version,
            DeadLetterTransition {
                classification: "dead_letter".into(),
                error: "ULTRA_SECRET_DISCORD_RESPONSE_TOKEN_4Jk72pXq".into(),
                duplicate_risk: false
            },
            21
        )
        .expect("dead")
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT terminal_error FROM provider_delivery_outbox WHERE id='one'",
            [],
            |row| row.get(0)
        )
        .expect("safe"),
        "provider delivery failed"
    );
}

#[test]
fn lease_version_fences_a_reused_owner_and_token_after_reclaim() {
    let mut conn = db();
    enqueue(&mut conn, event("one", "a"), "one", 10);
    let first = outbox::claim_next(&mut conn, "worker", "reused-token", 20)
        .expect("first")
        .expect("lease");
    let second = outbox::claim_next(&mut conn, "worker", "reused-token", 20 + LEASE_MILLIS + 1)
        .expect("reclaim")
        .expect("lease");
    assert_eq!(first.lease_version + 1, second.lease_version);
    assert!(
        !outbox::accepted(
            &conn,
            &first.id,
            "worker",
            "reused-token",
            first.lease_version,
            "late-v1",
            40
        )
        .expect("fenced")
    );
    assert!(
        outbox::accepted(
            &conn,
            &second.id,
            "worker",
            "reused-token",
            second.lease_version,
            "current-v2",
            41
        )
        .expect("current")
    );
}

#[test]
fn webhook_identifiers_classifications_and_idempotency_are_hardened() {
    let mut conn = db();
    let unsafe_target = EnqueueDelivery {
        target_key: "https://discord.com/api/webhooks/1/token".into(),
        ..event("url", "a")
    };
    let tx = conn.transaction().expect("tx");
    assert!(matches!(
        outbox::enqueue_in_transaction(&tx, &unsafe_target, "unsafe", 0),
        Err(AppError::Validation(_))
    ));
    drop(tx);
    let raw_secret = EnqueueDelivery {
        secret_ref: "raw-token-must-never-be-here".into(),
        ..event("raw-secret", "a")
    };
    let tx = conn.transaction().expect("tx");
    assert!(matches!(
        outbox::enqueue_in_transaction(&tx, &raw_secret, "raw-secret", 0),
        Err(AppError::Validation(_))
    ));
    drop(tx);
    enqueue(&mut conn, event("one", "a"), "one", 10);
    let changed = EnqueueDelivery {
        payload: b"different".to_vec(),
        ..event("one", "a")
    };
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox::enqueue_in_transaction(&tx, &changed, "changed", 11).expect_err("conflict"),
        AppError::Conflict("outbox idempotency conflict".into())
    );
    drop(tx);
    let leased = outbox::claim_next(&mut conn, "worker", "token", 20)
        .expect("claim")
        .expect("row");
    assert!(matches!(
        outbox::retry(
            &conn,
            &leased.id,
            "worker",
            "token",
            leased.lease_version,
            RetryTransition {
                next_attempt_at: 30,
                classification: "invalid_secret".into(),
                duplicate_risk: false
            },
            21
        ),
        Err(AppError::Validation(_))
    ));
    assert!(matches!(
        outbox::dead_letter(
            &conn,
            &leased.id,
            "worker",
            "token",
            leased.lease_version,
            DeadLetterTransition {
                classification: "network".into(),
                error: "network".into(),
                duplicate_risk: false
            },
            21
        ),
        Err(AppError::Validation(_))
    ));
    assert_eq!(
        outbox::dead_letter(
            &conn,
            &leased.id,
            "worker",
            "token",
            leased.lease_version,
            DeadLetterTransition {
                classification: "invalid_secret".into(),
                error: "invalid_secret".into(),
                duplicate_risk: true
            },
            21
        )
        .expect_err("ordinary terminal outcomes cannot claim duplicate risk"),
        AppError::Validation("duplicate risk is valid only for exhausted delivery".into())
    );
    assert!(matches!(
        outbox::retry(
            &conn,
            &leased.id,
            "worker",
            "token",
            leased.lease_version,
            RetryTransition {
                next_attempt_at: 30,
                classification: "https://discord.com/api/webhooks/token".into(),
                duplicate_risk: false
            },
            21
        ),
        Err(AppError::Validation(_))
    ));
}

#[test]
fn fixed_provider_outcomes_cover_retry_and_terminal_records() {
    let mut conn = db();
    enqueue(&mut conn, event("retry", "a"), "retry", 10);
    let retry = outbox::claim_next(&mut conn, "worker", "retry-token", 20)
        .expect("claim")
        .expect("row");
    assert!(
        outbox::retry(
            &conn,
            &retry.id,
            "worker",
            "retry-token",
            retry.lease_version,
            RetryTransition {
                next_attempt_at: 30,
                classification: "network".into(),
                duplicate_risk: false
            },
            21
        )
        .expect("network retry")
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT result_classification FROM provider_delivery_outbox WHERE id='retry'",
            [],
            |row| row.get(0)
        )
        .expect("classification"),
        "network"
    );
    enqueue(&mut conn, event("terminal", "b"), "terminal", 22);
    let terminal = outbox::claim_next(&mut conn, "worker", "terminal-token", 22)
        .expect("claim")
        .expect("row");
    assert!(
        outbox::dead_letter(
            &conn,
            &terminal.id,
            "worker",
            "terminal-token",
            terminal.lease_version,
            DeadLetterTransition {
                classification: "unknown_webhook".into(),
                error: "unknown_webhook".into(),
                duplicate_risk: false
            },
            23
        )
        .expect("dead")
    );
    let outcome: (String, String) = conn.query_row("SELECT result_classification, terminal_error FROM provider_delivery_outbox WHERE id='terminal'", [], |row| Ok((row.get(0)?, row.get(1)?))).expect("outcome");
    assert_eq!(
        outcome,
        ("unknown_webhook".into(), "unknown_webhook".into())
    );
}

#[test]
fn direct_exhausted_dead_letter_preserves_explicit_duplicate_risk() {
    let mut conn = db();
    enqueue(&mut conn, event("exhausted", "a"), "exhausted", 10);
    let first = outbox::claim_next(&mut conn, "worker", "token-1", 20)
        .expect("claim")
        .expect("row");
    assert!(
        !outbox::dead_letter(
            &conn,
            &first.id,
            "worker",
            "token-1",
            first.lease_version,
            DeadLetterTransition {
                classification: "attempts_exhausted".into(),
                error: "attempts_exhausted".into(),
                duplicate_risk: true
            },
            21
        )
        .expect("premature exhaustion is fenced")
    );
    assert!(
        outbox::retry(
            &conn,
            &first.id,
            "worker",
            "token-1",
            first.lease_version,
            RetryTransition {
                next_attempt_at: 20,
                classification: "network".into(),
                duplicate_risk: true
            },
            20
        )
        .expect("retry")
    );
    for attempt in 2..MAX_ATTEMPTS {
        let token = format!("token-{attempt}");
        let leased = outbox::claim_next(&mut conn, "worker", &token, 20)
            .expect("claim")
            .expect("row");
        assert!(
            outbox::retry(
                &conn,
                &leased.id,
                "worker",
                &token,
                leased.lease_version,
                RetryTransition {
                    next_attempt_at: 20,
                    classification: "network".into(),
                    duplicate_risk: true
                },
                20
            )
            .expect("retry")
        );
    }
    let leased = outbox::claim_next(&mut conn, "worker", "token-8", 20)
        .expect("claim")
        .expect("eighth attempt");
    assert_eq!(leased.attempts, MAX_ATTEMPTS);
    assert!(
        outbox::dead_letter(
            &conn,
            &leased.id,
            "worker",
            "token-8",
            leased.lease_version,
            DeadLetterTransition {
                classification: "attempts_exhausted".into(),
                error: "attempts_exhausted".into(),
                duplicate_risk: true
            },
            21
        )
        .expect("dead letter")
    );
    assert_eq!(
        conn.query_row::<(String, String, i64), _, _>(
            "SELECT state,result_classification,duplicate_risk
             FROM provider_delivery_outbox WHERE id='exhausted'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        )
        .expect("dead letter"),
        ("dead_letter".into(), "attempts_exhausted".into(), 1)
    );
}

#[test]
fn eighth_retry_dead_letters_frees_capacity_and_unblocks_target_fifo() {
    let mut conn = db();
    enqueue(&mut conn, event("exhaust", "a"), "exhaust", 10);
    enqueue(&mut conn, event("follower", "a"), "follower", 11);
    for attempt in 1..=MAX_ATTEMPTS {
        let token = format!("token-{attempt}");
        let leased = outbox::claim_next(&mut conn, "worker", &token, 20)
            .expect("claim")
            .expect("exhausted row");
        assert_eq!(leased.event_id, "exhaust");
        assert_eq!(leased.attempts, attempt);
        assert!(
            outbox::retry(
                &conn,
                &leased.id,
                "worker",
                &token,
                leased.lease_version,
                RetryTransition {
                    next_attempt_at: 20,
                    classification: "network".into(),
                    duplicate_risk: true
                },
                20
            )
            .expect("retry transition")
        );
    }
    let exhausted: (String, i64, String, String, i64) = conn
        .query_row(
            "SELECT state,attempts,result_classification,terminal_error,duplicate_risk
             FROM provider_delivery_outbox WHERE event_id='exhaust'",
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
        .expect("exhausted");
    assert_eq!(
        exhausted,
        (
            "dead_letter".into(),
            MAX_ATTEMPTS,
            "attempts_exhausted".into(),
            "attempts_exhausted".into(),
            1
        )
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM provider_delivery_outbox
             WHERE state IN ('blocked','ready','leased','retry')",
            [],
            |row| row.get(0)
        )
        .expect("active depth"),
        1,
        "the dead letter no longer consumes active capacity"
    );
    let follower = outbox::claim_next(&mut conn, "worker", "follower-token", 20)
        .expect("claim follower")
        .expect("follower");
    assert_eq!(follower.event_id, "follower");
}

#[test]
fn expired_eighth_lease_dead_letters_on_restart_and_releases_fifo() {
    let mut conn = db();
    enqueue(&mut conn, event("exhaust", "a"), "exhaust", 10);
    enqueue(&mut conn, event("follower", "a"), "follower", 11);
    for attempt in 1..MAX_ATTEMPTS {
        let token = format!("token-{attempt}");
        let leased = outbox::claim_next(&mut conn, "worker", &token, 20)
            .expect("claim")
            .expect("exhausted row");
        assert!(
            outbox::retry(
                &conn,
                &leased.id,
                "worker",
                &token,
                leased.lease_version,
                RetryTransition {
                    next_attempt_at: 20,
                    classification: "network".into(),
                    duplicate_risk: true
                },
                20
            )
            .expect("retry")
        );
    }
    let eighth = outbox::claim_next(&mut conn, "worker", "eighth-token", 20)
        .expect("eighth claim")
        .expect("eighth lease");
    assert_eq!(eighth.attempts, MAX_ATTEMPTS);
    let follower = outbox::claim_next(
        &mut conn,
        "replacement",
        "replacement-token",
        20 + LEASE_MILLIS + 1,
    )
    .expect("restart recovery")
    .expect("follower");
    assert_eq!(follower.event_id, "follower");
    let recovered: (String, String, i64) = conn
        .query_row(
            "SELECT state,result_classification,duplicate_risk
             FROM provider_delivery_outbox WHERE event_id='exhaust'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("recovered");
    assert_eq!(
        recovered,
        (
            "dead_letter".into(),
            "attempts_exhausted_after_worker_restart".into(),
            1
        )
    );
}
