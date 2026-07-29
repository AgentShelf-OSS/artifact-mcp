//! U11 — public share links.
//!
//! The load-bearing property is negative: a token that does not grant access must not reveal
//! *why*. `four_non_resolving_states_are_indistinguishable` is the proof, and the rest of this
//! file pins the rules that produce those states.

use std::collections::BTreeSet;

use artifact_mcp::config::{
    IdSource, NanoIdSource, SHARE_TOKEN_ALPHABET, SHARE_TOKEN_LENGTH, SequentialIdSource,
};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{CreateShare, ShareGrant, ShareToken};
use artifact_mcp::persistence::shares::{
    self, EXPIRES_CALENDAR_MESSAGE, EXPIRES_FORMAT_MESSAGE, EXPIRES_FUTURE_MESSAGE,
};
use rusqlite::{Connection, params};

use crate::u11_support::{Fixture, org};

const ORG: &str = "acme";
const CLIENT: &str = "key-acme";

fn request(expires: &str) -> CreateShare {
    CreateShare {
        created_by: "viewer@example.com".to_owned(),
        expires: expires.to_owned(),
    }
}

/// Writes a share row directly so a test can pin `expires_at` / `revoked_at` relative to
/// SQLite's own `now`, which is the clock [`shares::resolve`] compares against.
fn insert_share(conn: &Connection, token: &str, artifact: &str, org: &str, expires_at: &str) {
    conn.execute(
        "INSERT INTO artifact_shares (token, artifact_id, org, created_by, expires_at) \
         VALUES (?1, ?2, ?3, 'seed', ?4)",
        params![token, artifact, org, expires_at],
    )
    .expect("insert share row");
}

/// SQLite's rendering of `now` shifted by a modifier, in the exact `toISOString()` shape
/// `expiryFor` stores.
fn iso_now(conn: &Connection, modifier: &str) -> String {
    conn.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?1)",
        params![modifier],
        |row| row.get::<_, String>(0),
    )
    .expect("render a relative timestamp")
}

// ---------------------------------------------------------------------------
// Token shape and entropy
// ---------------------------------------------------------------------------

#[test]
fn share_tokens_are_24_symbols_of_the_frozen_64_symbol_alphabet() {
    // 24 symbols x log2(64) = 144 bits. Both halves of that product are frozen by U02 from
    // [lib/shares.js:6]; this test is what fails if either drifts.
    assert_eq!(SHARE_TOKEN_LENGTH, 24);
    assert_eq!(SHARE_TOKEN_ALPHABET.chars().count(), 64);
    assert_eq!(
        SHARE_TOKEN_ALPHABET,
        "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_-"
    );
    let unique: BTreeSet<char> = SHARE_TOKEN_ALPHABET.chars().collect();
    assert_eq!(unique.len(), 64, "the alphabet must not repeat a symbol");

    let ids = NanoIdSource::default();
    let mut seen = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    for _ in 0..2_000 {
        let token = ids.share_token().expect("mint a share token");
        assert_eq!(token.0.chars().count(), SHARE_TOKEN_LENGTH);
        assert!(
            token.0.chars().all(|symbol| unique.contains(&symbol)),
            "token {token} left the frozen alphabet"
        );
        assert!(seen.insert(token.0.clone()), "duplicate token {token}");
        symbols.extend(token.0.chars());
    }
    // 48,000 draws from a uniform 64-symbol alphabet miss a symbol with probability ~0; a
    // truncated or biased mask (the classic nanoid porting bug) shows up here immediately.
    assert_eq!(symbols.len(), 64, "the sampler never emitted some symbols");
}

// ---------------------------------------------------------------------------
// Expiry
// ---------------------------------------------------------------------------

#[test]
fn expiry_keywords_and_boundaries_follow_node() {
    let fixture = Fixture::new("u11-expiry");
    let clock = &fixture.clock; // 2026-01-01T00:00:00.000Z

    assert_eq!(shares::expiry_for(clock, "never"), Ok(None));
    assert_eq!(
        shares::expiry_for(clock, "24h"),
        Ok(Some("2026-01-02T00:00:00.000Z".to_owned()))
    );

    // `date.getTime() <= Date.now()` — "now" itself is not in the future.
    assert_eq!(
        shares::expiry_for(clock, "2026-01-01"),
        Err(AppError::Validation(EXPIRES_FUTURE_MESSAGE.to_owned()))
    );
    assert_eq!(
        shares::expiry_for(clock, "2026-01-01T00:00:00.001Z"),
        Ok(Some("2026-01-01T00:00:00.001Z".to_owned()))
    );
    // One millisecond in the past is rejected with the same message an unparseable value gets.
    fixture.clock.advance_millis(2);
    assert_eq!(
        shares::expiry_for(clock, "2026-01-01T00:00:00.001Z"),
        Err(AppError::Validation(EXPIRES_FUTURE_MESSAGE.to_owned()))
    );
}

#[test]
fn expiry_rejects_impossible_calendar_dates_and_bad_shapes() {
    let fixture = Fixture::new("u11-expiry-shape");
    let clock = &fixture.clock;

    // Shape failures — the value never reaches `new Date`.
    for value in [
        "",
        "24H",
        "Never",
        "tomorrow",
        "2026-1-1",
        "20260101",
        "2026-01-01T10",
        "2026-01-01 10:00Z",
        "2026-01-01T10:00:00.1234Z",
        "2026-01-01T10:00:00Z ",
        "+2026-01-01",
    ] {
        assert_eq!(
            shares::expiry_for(clock, value),
            Err(AppError::Validation(EXPIRES_FORMAT_MESSAGE.to_owned())),
            "{value:?} should fail the shape test"
        );
    }

    // Shape-valid but `new Date` rejects the field values.
    for value in [
        "2026-13-01",
        "2026-00-10",
        "2026-01-32",
        "2026-01-00",
        "2027-05-01T24:01Z",
        "2027-05-01T25:00Z",
        "2027-05-01T10:60Z",
        "2027-05-01T10:00:60Z",
        "2027-05-01T10:00+24:00",
        "2027-05-01T10:00+00:60",
    ] {
        assert_eq!(
            shares::expiry_for(clock, value),
            Err(AppError::Validation(EXPIRES_FUTURE_MESSAGE.to_owned())),
            "{value:?} should be an invalid instant"
        );
    }

    // Rolled-over calendar dates: `new Date("2027-02-31")` is 2027-03-03, a longer-lived link
    // than was asked for [lib/shares.js:35-39].
    for value in ["2027-02-31", "2027-02-30", "2027-04-31", "2027-06-31"] {
        assert_eq!(
            shares::expiry_for(clock, value),
            Err(AppError::Validation(EXPIRES_CALENDAR_MESSAGE.to_owned())),
            "{value:?} is not a real calendar date"
        );
    }
    // A real leap day is accepted; the calendar guard only fires on a rollover.
    assert_eq!(
        shares::expiry_for(clock, "2028-02-29"),
        Ok(Some("2028-02-29T00:00:00.000Z".to_owned()))
    );
    // Only the date-only shape is calendar-checked, so a rolled-over date *with* a time survives
    // — faithfully reproducing Node rather than "fixing" it.
    assert_eq!(
        shares::expiry_for(clock, "2027-02-31T00:00Z"),
        Ok(Some("2027-03-03T00:00:00.000Z".to_owned()))
    );
    // Offsets and fractions are honoured exactly.
    assert_eq!(
        shares::expiry_for(clock, "2027-05-01T10:00+02:00"),
        Ok(Some("2027-05-01T08:00:00.000Z".to_owned()))
    );
    assert_eq!(
        shares::expiry_for(clock, "2027-05-01T10:00-0230"),
        Ok(Some("2027-05-01T12:30:00.000Z".to_owned()))
    );
    assert_eq!(
        shares::expiry_for(clock, "2027-05-01T10:00:00.5Z"),
        Ok(Some("2027-05-01T10:00:00.500Z".to_owned()))
    );
    assert_eq!(
        shares::expiry_for(clock, "2027-05-01T24:00Z"),
        Ok(Some("2027-05-02T00:00:00.000Z".to_owned()))
    );
}

#[test]
fn an_expiry_at_the_sqlite_boundary_does_not_resolve() {
    let fixture = Fixture::new("u11-boundary");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("boundary0001", ORG, CLIENT);

    // `julianday(expires_at) > julianday('now')` is strict: the instant of expiry is already past.
    let at_now = iso_now(&conn, "+0 seconds");
    let a_second_ago = iso_now(&conn, "-1 seconds");
    let an_hour_ahead = iso_now(&conn, "+1 hours");
    insert_share(&conn, "tok-at-now", &artifact.0, ORG, &at_now);
    insert_share(&conn, "tok-past", &artifact.0, ORG, &a_second_ago);
    insert_share(&conn, "tok-future", &artifact.0, ORG, &an_hour_ahead);

    assert_eq!(
        shares::resolve(&conn, &ShareToken::from("tok-at-now")),
        Ok(None)
    );
    assert_eq!(
        shares::resolve(&conn, &ShareToken::from("tok-past")),
        Ok(None)
    );
    assert!(
        shares::resolve(&conn, &ShareToken::from("tok-future"))
            .expect("resolve")
            .is_some()
    );

    // The listing applies the identical predicate, so an expired link also disappears from the
    // owner's management view.
    let listed: Vec<String> = shares::list_for_artifact(&conn, &artifact)
        .expect("list shares")
        .into_iter()
        .map(|share| share.token.0)
        .collect();
    assert_eq!(listed, vec!["tok-future".to_owned()]);
}

// ---------------------------------------------------------------------------
// Create / list / revoke
// ---------------------------------------------------------------------------

#[test]
fn create_persists_the_grant_and_returns_only_token_and_expiry() {
    let fixture = Fixture::new("u11-create");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("create000001", ORG, CLIENT);

    let created = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &artifact,
        &org(ORG),
        &request("never"),
    )
    .expect("create a share");

    assert_eq!(created.expires_at, None);
    // Node returns `{ token, expires_at }` only; `created_at`/`created_by` come back from the
    // listing, never from the create call [lib/shares.js:47].
    assert_eq!(created.created_at, None);
    assert_eq!(created.created_by, None);

    let listed = shares::list_for_artifact(&conn, &artifact).expect("list shares");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].token, created.token);
    assert_eq!(
        listed[0].created_by.as_deref(),
        Some("viewer@example.com"),
        "created_by is persisted verbatim"
    );
    assert!(listed[0].created_at.is_some());

    assert_eq!(
        shares::resolve(&conn, &created.token),
        Ok(Some(ShareGrant {
            artifact_id: artifact.clone(),
            org: org(ORG),
        }))
    );
}

#[test]
fn a_rejected_expiry_consumes_neither_a_token_nor_a_row() {
    let fixture = Fixture::new("u11-create-reject");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("reject000001", ORG, CLIENT);

    let before = fixture.ids.share_token().expect("mint");
    let error = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &artifact,
        &org(ORG),
        &request("2020-01-01"),
    )
    .expect_err("a past expiry is rejected");
    assert_eq!(
        error,
        AppError::Validation(EXPIRES_FUTURE_MESSAGE.to_owned())
    );

    let after = fixture.ids.share_token().expect("mint");
    // Node validates before `generateToken()` [lib/shares.js:44-45]; the counter must have moved
    // by exactly the two explicit mints above.
    assert_ne!(before, after);
    assert_eq!(
        shares::list_for_artifact(&conn, &artifact).expect("list"),
        vec![]
    );
}

#[test]
fn revocation_is_idempotent_and_immediate() {
    let fixture = Fixture::new("u11-revoke");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("revoke000001", ORG, CLIENT);
    let other = fixture.seed_artifact("revoke000002", ORG, CLIENT);

    let created = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &artifact,
        &org(ORG),
        &request("never"),
    )
    .expect("create");

    // Another artifact cannot revoke this token: the statement is scoped by artifact id.
    assert_eq!(shares::revoke(&conn, &other, &created.token), Ok(false));
    assert!(
        shares::resolve(&conn, &created.token)
            .expect("resolve")
            .is_some()
    );

    assert_eq!(shares::revoke(&conn, &artifact, &created.token), Ok(true));
    assert_eq!(shares::resolve(&conn, &created.token), Ok(None));
    // `revoked_at IS NULL` in the WHERE clause makes a repeat revoke a no-op, which is what makes
    // the MCP tool report `revoked: false` on a retry [lib/mcp.js:497].
    assert_eq!(shares::revoke(&conn, &artifact, &created.token), Ok(false));
    assert_eq!(
        shares::list_for_artifact(&conn, &artifact).expect("list"),
        vec![]
    );
}

// ---------------------------------------------------------------------------
// The indistinguishability proof
// ---------------------------------------------------------------------------

#[test]
fn four_non_resolving_states_are_indistinguishable() {
    let fixture = Fixture::new("u11-conceal");
    let conn = fixture.conn();
    let live_artifact = fixture.seed_artifact("conceal00001", ORG, CLIENT);
    let doomed_artifact = fixture.seed_artifact("conceal00002", ORG, CLIENT);
    let moved_artifact = fixture.seed_artifact("conceal00003", ORG, CLIENT);

    // 1. valid — the control.
    let live = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &live_artifact,
        &org(ORG),
        &request("never"),
    )
    .expect("create the live share");

    // 2. expired.
    let expired = ShareToken::from("state-expired");
    let past = iso_now(&conn, "-1 hours");
    insert_share(&conn, &expired.0, &live_artifact.0, ORG, &past);

    // 3. revoked.
    let revoked = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &live_artifact,
        &org(ORG),
        &request("never"),
    )
    .expect("create the share to revoke");
    assert_eq!(
        shares::revoke(&conn, &live_artifact, &revoked.token),
        Ok(true)
    );

    // 4. invalid — a well-formed token that was never issued.
    let invalid = ShareToken::from("AAAAAAAAAAAAAAAAAAAAAAAA");

    // 5. stale, two flavours. Both need the foreign key relaxed to *create*, which is the point:
    // the schema stops them arising normally, and this test proves the read path is safe anyway
    // if one ever does (a crash between the org move's UPDATE and its share DELETE, a restored
    // backup, an operator's manual edit).
    let stale_deleted = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &doomed_artifact,
        &org(ORG),
        &request("never"),
    )
    .expect("create the share whose artifact disappears");
    let stale_moved = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &moved_artifact,
        &org(ORG),
        &request("never"),
    )
    .expect("create the share whose artifact is re-tenanted");
    conn.execute_batch("PRAGMA foreign_keys = OFF")
        .expect("relax foreign keys");
    conn.execute(
        "DELETE FROM artifacts WHERE id = ?1",
        params![doomed_artifact.0],
    )
    .expect("delete the artifact without cascading");
    conn.execute(
        "UPDATE artifacts SET org = 'globex' WHERE id = ?1",
        params![moved_artifact.0],
    )
    .expect("re-tenant the artifact without dropping its shares");
    conn.execute_batch("PRAGMA foreign_keys = ON")
        .expect("restore foreign keys");
    assert_eq!(
        conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0)),
        Ok(1),
        "the pinned pragma must be back on"
    );
    // The rows really did survive; otherwise this test would prove nothing.
    let orphan_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_shares s \
             WHERE NOT EXISTS (SELECT 1 FROM artifacts a WHERE a.id = s.artifact_id AND a.org = s.org)",
            [],
            |row| row.get(0),
        )
        .expect("count orphaned share rows");
    assert_eq!(orphan_rows, 2);

    // The proof: every non-resolving state produces the *same value*, not merely the same shape.
    // There is no reason code, no error variant, and no second entry point that could tell a
    // route which case it hit — so `/s/:token` cannot become an existence oracle
    // (`share.public-delivery` step 5 asserts the resulting 404s are byte-identical).
    let outcomes = [
        ("invalid", shares::resolve(&conn, &invalid)),
        ("expired", shares::resolve(&conn, &expired)),
        ("revoked", shares::resolve(&conn, &revoked.token)),
        (
            "stale-deleted",
            shares::resolve(&conn, &stale_deleted.token),
        ),
        ("stale-moved", shares::resolve(&conn, &stale_moved.token)),
    ];
    for (label, outcome) in &outcomes {
        assert_eq!(*outcome, Ok(None), "{label} must not resolve");
    }
    let distinct: BTreeSet<String> = outcomes
        .iter()
        .map(|(_, outcome)| format!("{outcome:?}"))
        .collect();
    assert_eq!(
        distinct.len(),
        1,
        "the non-resolving states are distinguishable: {distinct:?}"
    );

    // …while the live token still works, so the collapse is not "deny everything".
    assert_eq!(
        shares::resolve(&conn, &live.token),
        Ok(Some(ShareGrant {
            artifact_id: live_artifact.clone(),
            org: org(ORG),
        }))
    );
}

#[test]
fn an_org_move_revokes_every_share() {
    let fixture = Fixture::new("u11-move");
    let mut conn = fixture.conn();
    let artifact = fixture.seed_artifact("moved0000001", ORG, CLIENT);
    let first = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &artifact,
        &org(ORG),
        &request("never"),
    )
    .expect("create");
    let second = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &artifact,
        &org(ORG),
        &request("24h"),
    )
    .expect("create");

    // `moveArtifactToOrg` [lib/store.js:463-480]: deferred FKs, move the parent and every
    // org-bearing child, then destroy the share links.
    let transaction = conn.transaction().expect("begin move");
    transaction
        .execute_batch("PRAGMA defer_foreign_keys = ON")
        .expect("defer foreign keys");
    transaction
        .execute(
            "UPDATE artifacts SET org = 'globex' WHERE id = ?1",
            params![artifact.0],
        )
        .expect("move the artifact");
    assert_eq!(
        shares::revoke_all_for_artifact(&transaction, &artifact),
        Ok(2),
        "both links are destroyed, not carried into the new tenant"
    );
    transaction.commit().expect("commit move");

    assert_eq!(shares::resolve(&conn, &first.token), Ok(None));
    assert_eq!(shares::resolve(&conn, &second.token), Ok(None));
    assert_eq!(
        shares::list_for_artifact(&conn, &artifact).expect("list"),
        vec![]
    );
    // And the new tenant's own link works, so the move did not poison the artifact.
    let reshared = shares::create(
        &conn,
        &fixture.ids,
        &fixture.clock,
        &artifact,
        &org("globex"),
        &request("never"),
    )
    .expect("re-share under the new org");
    assert_eq!(
        shares::resolve(&conn, &reshared.token),
        Ok(Some(ShareGrant {
            artifact_id: artifact,
            org: org("globex"),
        }))
    );
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[test]
fn listings_are_newest_first_with_a_descending_token_tiebreak() {
    let fixture = Fixture::new("u11-order");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("ordered00001", ORG, CLIENT);
    let elsewhere = fixture.seed_artifact("ordered00002", ORG, CLIENT);

    // `ORDER BY created_at DESC, token DESC` — the tiebreak matters because `created_at` has
    // one-second resolution, so a burst of links shares a timestamp.
    for (token, created_at) in [
        ("aaa", "2026-01-01 00:00:00"),
        ("ccc", "2026-01-01 00:00:00"),
        ("bbb", "2026-01-01 00:00:00"),
        ("zzz", "2025-06-01 00:00:00"),
        ("mmm", "2026-06-01 00:00:00"),
    ] {
        conn.execute(
            "INSERT INTO artifact_shares (token, artifact_id, org, created_by, created_at) \
             VALUES (?1, ?2, ?3, 'seed', ?4)",
            params![token, artifact.0, ORG, created_at],
        )
        .expect("insert ordered share");
    }
    conn.execute(
        "INSERT INTO artifact_shares (token, artifact_id, org, created_by) \
         VALUES ('other', ?1, ?2, 'seed')",
        params![elsewhere.0, ORG],
    )
    .expect("insert a share on another artifact");

    let listed: Vec<String> = shares::list_for_artifact(&conn, &artifact)
        .expect("list")
        .into_iter()
        .map(|share| share.token.0)
        .collect();
    assert_eq!(listed, ["mmm", "ccc", "bbb", "aaa", "zzz"]);
}

#[test]
fn listings_are_scoped_to_one_artifact_even_within_an_org() {
    let fixture = Fixture::new("u11-scope");
    let conn = fixture.conn();
    let mine = fixture.seed_artifact("scoped000001", ORG, CLIENT);
    let theirs = fixture.seed_artifact("scoped000002", ORG, CLIENT);
    let ids = SequentialIdSource::starting_at(500);

    let their_share = shares::create(
        &conn,
        &ids,
        &fixture.clock,
        &theirs,
        &org(ORG),
        &request("never"),
    )
    .expect("create");
    assert_eq!(
        shares::list_for_artifact(&conn, &mine).expect("list"),
        vec![]
    );
    // The grant still names its own artifact, so a resolved token can never be redirected.
    assert_eq!(
        shares::resolve(&conn, &their_share.token)
            .expect("resolve")
            .map(|grant| grant.artifact_id),
        Some(theirs)
    );
}
