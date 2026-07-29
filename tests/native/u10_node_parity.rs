//! U10 cross-runtime proof: the Rust engagement adapters must agree with `lib/reactions.js`,
//! `lib/views.js`, `lib/notifications.js`, and `lib/contracts.js` on ordering, aggregate shapes,
//! written rows, and error strings.
//!
//! Every assertion here drives the *real* Node modules through `node -e`
//! (see `u10_support::run_node`), because a Rust-only round trip cannot prove that the ported SQL
//! keeps Node's row order or that a rejection message is byte-identical. Later HTTP/MCP
//! conformance compares ordered JSON arrays (`artifact_stats` in `lib/mcp.js:502`, the gallery
//! notification feed in `lib/app.js:229-232`), so ordering is a contract, not an implementation
//! detail.
//!
//! Read parity runs both runtimes against the *same* SQLite file. Write parity uses two
//! identically seeded databases, applies the same operation sequence to each, and compares the
//! resulting rows.
//!
//! These tests skip when Node is unavailable; `REQUIRE_NODE_REFERENCE=1` turns that into a hard
//! failure, as the U01 contract requires.

use artifact_mcp::model::{ArtifactId, EmailAddress, OrgId, ReactionUpdate, Viewer};
use artifact_mcp::persistence::{notifications, reactions, views};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::u10_support::{
    Fixture, as_json, column, insert_artifact, insert_feedback, insert_reaction, insert_view,
    node_op, node_reference_available, run_node,
};

const PAST: &str = "2000-01-01 00:00:00";

fn id(value: &str) -> ArtifactId {
    ArtifactId::from(value)
}

fn email(value: &str) -> EmailAddress {
    EmailAddress::from(value)
}

fn org(value: &str) -> OrgId {
    OrgId::from(value)
}

/// Deterministic view analytics: distinct counts and timestamps, plus a `views` tie that only the
/// `last_viewed_at` secondary sort can break.
fn seed_views(conn: &Connection) {
    for (artifact, tenant, title) in [
        ("alpha", "acme", "Alpha"),
        ("bravo", "acme", "Bravo"),
        ("charlie", "acme", "Charlie"),
        ("delta", "acme", "Delta"),
        ("echo", "other", "Echo"),
    ] {
        insert_artifact(conn, artifact, tenant, title);
    }
    // alpha: three viewers, staggered recency.
    insert_view(
        conn,
        "alpha",
        "acme",
        "first@x.test",
        2,
        "2026-01-01 00:00:00",
        "2026-01-01 09:00:00",
    );
    insert_view(
        conn,
        "alpha",
        "acme",
        "last@x.test",
        1,
        "2026-01-02 00:00:00",
        "2026-01-02 09:00:00",
    );
    insert_view(
        conn,
        "alpha",
        "acme",
        "middle@x.test",
        4,
        "2026-01-01 12:00:00",
        "2026-01-01 18:00:00",
    );
    // bravo ties alpha on total views (7) and must be ordered by recency.
    insert_view(
        conn,
        "bravo",
        "acme",
        "solo@x.test",
        7,
        "2026-02-01 00:00:00",
        "2026-02-01 00:00:00",
    );
    insert_view(
        conn,
        "charlie",
        "acme",
        "solo@x.test",
        9,
        "2026-01-05 00:00:00",
        "2026-01-05 00:00:00",
    );
    insert_view(conn, "delta", "acme", "solo@x.test", 1, PAST, PAST);
    insert_view(conn, "echo", "other", "solo@x.test", 99, PAST, PAST);
}

/// Rust's `counts_for_org` projection in the oracle's `[id, views, unique_viewers]` shape.
fn rust_org_counts(conn: &Connection, tenant: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = views::counts_for_org(conn, &org(tenant))
        .expect("org counts")
        .into_iter()
        .map(|(artifact, counts)| json!([artifact.0, counts.views, counts.unique_viewers]))
        .collect();
    rows.sort_by_key(std::string::ToString::to_string);
    rows
}

fn sorted(mut rows: Vec<Value>) -> Vec<Value> {
    rows.sort_by_key(std::string::ToString::to_string);
    rows
}

#[test]
fn node_and_rust_agree_on_view_projections() {
    if !node_reference_available() {
        return;
    }
    let fixture = Fixture::new("parity-views");
    let conn = fixture.conn();
    seed_views(&conn);

    let node = run_node(
        fixture.path(),
        vec![
            json!({ "kind": "countsFor", "id": "alpha" }),
            json!({ "kind": "countsFor", "id": "never-viewed" }),
            json!({ "kind": "viewersFor", "id": "alpha" }),
            json!({ "kind": "countsForOrg", "org": "acme" }),
            json!({ "kind": "topForOrg", "org": "acme" }),
            json!({ "kind": "topForOrg", "org": "acme", "limit": 2 }),
            json!({ "kind": "topForOrg", "org": "acme", "limit": 0 }),
            json!({ "kind": "topForOrg", "org": "ghost" }),
        ],
    );

    assert_eq!(
        as_json(&views::counts_for(&conn, &id("alpha")).expect("counts")),
        node[0]
    );
    assert_eq!(
        as_json(&views::counts_for(&conn, &id("never-viewed")).expect("counts")),
        node[1],
        "an artifact with no rows must report Node's zeroed aggregate, not an absent row"
    );

    let rust_viewers = views::viewers_for(&conn, &id("alpha")).expect("viewers");
    assert_eq!(as_json(&rust_viewers), node[2]);
    assert_eq!(
        rust_viewers
            .iter()
            .map(|row| row.email.0.as_str())
            .collect::<Vec<_>>(),
        ["last@x.test", "middle@x.test", "first@x.test"],
        "ORDER BY last_viewed_at DESC, proved against the oracle above"
    );

    assert_eq!(
        rust_org_counts(&conn, "acme"),
        sorted(node[3].as_array().cloned().expect("org counts array"))
    );

    // topForOrg: default limit, explicit limit, falsy limit, and an empty tenant.
    assert_eq!(
        as_json(&views::top_for_org(&conn, &org("acme"), views::DEFAULT_TOP_LIMIT).expect("top")),
        node[4]
    );
    assert_eq!(
        as_json(&views::top_for_org(&conn, &org("acme"), 2).expect("top")),
        node[5]
    );
    assert_eq!(
        as_json(&views::top_for_org(&conn, &org("acme"), 0).expect("top")),
        node[6],
        "`Math.max(1, Number(limit) || 10)`: a falsy limit is the default, not an empty page"
    );
    assert_eq!(
        as_json(&views::top_for_org(&conn, &org("ghost"), 10).expect("top")),
        node[7]
    );

    // The tie on total views must be broken by recency in both runtimes.
    let order: Vec<String> = node[4]
        .as_array()
        .expect("top array")
        .iter()
        .map(|row| row["artifact_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(order, ["charlie", "bravo", "alpha", "delta"]);
}

#[test]
fn node_and_rust_agree_on_reaction_reads() {
    if !node_reference_available() {
        return;
    }
    let fixture = Fixture::new("parity-react-read");
    let conn = fixture.conn();
    insert_artifact(&conn, "alpha", "acme", "Alpha");
    insert_artifact(&conn, "bravo", "acme", "Bravo");
    insert_reaction(&conn, "a@x.test", "alpha", 1, 1);
    insert_reaction(&conn, "b@x.test", "alpha", 1, -1);
    insert_reaction(&conn, "c@x.test", "alpha", 0, 1);
    insert_reaction(&conn, "a@x.test", "bravo", 0, 0);

    let node = run_node(
        fixture.path(),
        vec![
            json!({ "kind": "getReaction", "email": "a@x.test", "id": "alpha" }),
            json!({ "kind": "getReaction", "email": "nobody@x.test", "id": "alpha" }),
            json!({ "kind": "reactionsFor", "email": "a@x.test" }),
            json!({ "kind": "sentimentMap" }),
        ],
    );

    assert_eq!(
        as_json(&reactions::get(&conn, &email("a@x.test"), &id("alpha")).expect("get")),
        node[0]
    );
    assert_eq!(
        as_json(&reactions::get(&conn, &email("nobody@x.test"), &id("alpha")).expect("get")),
        node[1],
        "a missing row reads as favorite 0 / vote 0 in both runtimes"
    );

    let mine: Vec<Value> = reactions::for_viewer(&conn, &email("a@x.test"))
        .expect("for_viewer")
        .into_iter()
        .map(|(artifact, reaction)| json!([artifact.0, reaction.favorite, reaction.vote]))
        .collect();
    assert_eq!(
        mine,
        sorted(node[2].as_array().cloned().expect("reactions array"))
    );

    let sentiment: Vec<Value> = reactions::sentiment(&conn)
        .expect("sentiment")
        .into_iter()
        .map(|(artifact, value)| json!([artifact.0, value.up, value.down, value.favorites]))
        .collect();
    assert_eq!(
        sentiment,
        sorted(node[3].as_array().cloned().expect("sentiment array"))
    );
}

#[test]
fn node_and_rust_agree_on_notification_projections() {
    if !node_reference_available() {
        return;
    }
    let fixture = Fixture::new("parity-notify");
    let conn = fixture.conn();
    insert_artifact(&conn, "artifact-a", "acme", "Acme report");
    insert_artifact(&conn, "artifact-b", "beta", "Beta report");
    // Three comments share a second so the `f.id DESC` tiebreaker is exercised.
    for suffix in ["a", "b", "c"] {
        insert_feedback(
            &conn,
            &format!("feedback-{suffix}"),
            "artifact-a",
            "acme",
            "author@acme.test",
            "Acme note",
            "2026-07-14 10:00:00",
        );
    }
    insert_feedback(
        &conn,
        "feedback-beta",
        "artifact-b",
        "beta",
        "author@beta.test",
        "Beta note",
        "2026-07-14 11:00:00",
    );
    insert_feedback(
        &conn,
        "feedback-mine",
        "artifact-a",
        "acme",
        "viewer@acme.test",
        "My own note",
        "2026-07-14 12:00:00",
    );

    let member = Viewer {
        email: Some(email("viewer@acme.test")),
        org: Some(org("acme")),
        is_admin: false,
    };
    let administrator = Viewer {
        email: Some(email("admin@example.test")),
        org: Some(org("admin")),
        is_admin: true,
    };

    let node = run_node(
        fixture.path(),
        vec![
            json!({ "kind": "recentForViewer", "email": "viewer@acme.test", "org": "acme", "isAdmin": false }),
            json!({ "kind": "recentForViewer", "email": "admin@example.test", "org": "admin", "isAdmin": true }),
            json!({ "kind": "recentForViewer", "email": "viewer@acme.test", "org": "acme", "isAdmin": false, "limit": 2 }),
            json!({ "kind": "unreadCount", "email": "viewer@acme.test", "org": "acme", "isAdmin": false }),
            json!({ "kind": "unreadCount", "email": "admin@example.test", "org": "admin", "isAdmin": true }),
        ],
    );

    let rust_member =
        notifications::recent_for_viewer(&conn, &member, notifications::DEFAULT_LIMIT)
            .expect("member notifications");
    assert_eq!(as_json(&rust_member), node[0]);
    assert_eq!(
        rust_member
            .iter()
            .map(|row| row.id.0.as_str())
            .collect::<Vec<_>>(),
        ["feedback-c", "feedback-b", "feedback-a"],
        "self-authored feedback is excluded and same-second ties fall back to f.id DESC"
    );

    assert_eq!(
        as_json(
            &notifications::recent_for_viewer(&conn, &administrator, notifications::DEFAULT_LIMIT)
                .expect("admin notifications")
        ),
        node[1],
        "the admin statement has no org filter at all"
    );
    assert_eq!(
        as_json(&notifications::recent_for_viewer(&conn, &member, 2).expect("limited")),
        node[2]
    );
    assert_eq!(
        json!(notifications::unread_count(&conn, &member).expect("unread")),
        node[3]
    );
    assert_eq!(
        json!(notifications::unread_count(&conn, &administrator).expect("unread")),
        node[4]
    );
}

#[test]
fn node_and_rust_agree_after_the_watermark_moves() {
    if !node_reference_available() {
        return;
    }
    let fixture = Fixture::new("parity-watermark");
    let conn = fixture.conn();
    insert_artifact(&conn, "artifact-a", "acme", "Acme report");
    insert_feedback(
        &conn,
        "feedback-a",
        "artifact-a",
        "acme",
        "author@acme.test",
        "Acme note",
        PAST,
    );
    let member = Viewer {
        email: Some(email("viewer@acme.test")),
        org: Some(org("acme")),
        is_admin: false,
    };

    // Node writes the watermark; Rust must read it the same way, and vice versa.
    let node = run_node(
        fixture.path(),
        vec![
            json!({ "kind": "unreadCount", "email": "viewer@acme.test", "org": "acme", "isAdmin": false }),
            json!({ "kind": "markSeen", "email": "viewer@acme.test" }),
            json!({ "kind": "unreadCount", "email": "viewer@acme.test", "org": "acme", "isAdmin": false }),
        ],
    );
    assert_eq!(node[0], json!(1));
    assert_eq!(node[2], json!(0));
    assert_eq!(
        notifications::unread_count(&conn, &member).expect("unread"),
        0,
        "Rust must honour a watermark Node wrote"
    );
    assert!(
        !notifications::recent_for_viewer(&conn, &member, notifications::DEFAULT_LIMIT)
            .expect("notifications")[0]
            .unread
    );

    insert_feedback(
        &conn,
        "feedback-later",
        "artifact-a",
        "acme",
        "author@acme.test",
        "Later note",
        "2999-01-01 00:00:00",
    );
    let after = node_op(
        fixture.path(),
        json!({ "kind": "unreadCount", "email": "viewer@acme.test", "org": "acme", "isAdmin": false }),
    );
    assert_eq!(after, json!(1));
    assert_eq!(
        notifications::unread_count(&conn, &member).expect("unread"),
        1
    );

    // Now the reverse direction: Rust marks seen, Node must read the same watermark back. The
    // future-dated comment stays unread in both runtimes because it postdates any `datetime('now')`.
    conn.execute("DELETE FROM notification_reads", [])
        .expect("reset watermark");
    notifications::mark_seen(&conn, &email("viewer@acme.test")).expect("mark seen");
    let reverse = run_node(
        fixture.path(),
        vec![
            json!({ "kind": "unreadCount", "email": "viewer@acme.test", "org": "acme", "isAdmin": false }),
            json!({ "kind": "recentForViewer", "email": "viewer@acme.test", "org": "acme", "isAdmin": false }),
        ],
    );
    assert_eq!(
        reverse[0],
        json!(1),
        "Node must honour a watermark Rust wrote: only the future-dated comment is still unread"
    );
    assert_eq!(
        reverse[1]
            .as_array()
            .expect("notification array")
            .iter()
            .map(|row| (
                row["id"].as_str().unwrap_or_default().to_owned(),
                row["unread"].as_bool().unwrap_or_default()
            ))
            .collect::<Vec<_>>(),
        [
            ("feedback-later".to_owned(), true),
            ("feedback-a".to_owned(), false)
        ]
    );
    assert_eq!(
        as_json(
            &notifications::recent_for_viewer(&conn, &member, notifications::DEFAULT_LIMIT)
                .expect("notifications")
        ),
        reverse[1],
        "and the Rust projection of that same watermark is identical"
    );
}

#[test]
fn node_and_rust_write_identical_reaction_rows() {
    if !node_reference_available() {
        return;
    }
    let node_fixture = Fixture::new("parity-react-node");
    let rust_fixture = Fixture::new("parity-react-rust");
    for fixture in [&node_fixture, &rust_fixture] {
        let conn = fixture.conn();
        insert_artifact(&conn, "alpha", "acme", "Alpha");
        insert_artifact(&conn, "bravo", "acme", "Bravo");
    }

    // The same partial-update script on both sides, including the "field omitted" cases that
    // exercise the read-modify-write in `lib/reactions.js:26-32`.
    let updates: Vec<(&str, &str, ReactionUpdate, Value)> = vec![
        (
            "a@x.test",
            "alpha",
            ReactionUpdate {
                favorite: Some(true),
                vote: Some(1),
            },
            json!({ "favorite": 1, "vote": 1 }),
        ),
        (
            "a@x.test",
            "alpha",
            ReactionUpdate {
                favorite: None,
                vote: Some(-1),
            },
            json!({ "vote": -1 }),
        ),
        (
            "a@x.test",
            "alpha",
            ReactionUpdate {
                favorite: Some(false),
                vote: None,
            },
            json!({ "favorite": 0 }),
        ),
        (
            "a@x.test",
            "alpha",
            ReactionUpdate {
                favorite: None,
                vote: Some(0),
            },
            json!({ "vote": 0 }),
        ),
        (
            "b@x.test",
            "alpha",
            ReactionUpdate {
                favorite: Some(true),
                vote: None,
            },
            json!({ "favorite": 1 }),
        ),
        (
            "a@x.test",
            "bravo",
            ReactionUpdate {
                favorite: Some(true),
                vote: Some(-1),
            },
            json!({ "favorite": 1, "vote": -1 }),
        ),
    ];

    let node_ops: Vec<Value> = updates
        .iter()
        .map(|(viewer, artifact, _, payload)| {
            json!({ "kind": "setReaction", "email": viewer, "id": artifact, "update": payload })
        })
        .collect();
    let node_results = run_node(node_fixture.path(), node_ops);

    let mut conn = rust_fixture.conn();
    for (index, (viewer, artifact, update, _)) in updates.into_iter().enumerate() {
        let stored =
            reactions::set(&mut conn, &email(viewer), &id(artifact), update).expect("set reaction");
        assert_eq!(
            as_json(&stored),
            node_results[index],
            "step {index} returned a different reaction than the Node reference"
        );
    }

    let dump = "SELECT email || '|' || artifact_id || '|' || favorite || '|' || vote \
                FROM reactions ORDER BY email, artifact_id";
    assert_eq!(
        column(&conn, dump),
        column(&node_fixture.conn(), dump),
        "the persisted rows must match the Node reference exactly"
    );
    assert_eq!(
        column(&conn, dump),
        [
            "a@x.test|alpha|0|0",
            "a@x.test|bravo|1|-1",
            "b@x.test|alpha|1|0"
        ],
        "one row per (viewer, artifact) after six updates"
    );
}

#[test]
fn node_and_rust_write_identical_view_rows() {
    if !node_reference_available() {
        return;
    }
    let node_fixture = Fixture::new("parity-view-node");
    let rust_fixture = Fixture::new("parity-view-rust");
    for fixture in [&node_fixture, &rust_fixture] {
        let conn = fixture.conn();
        insert_artifact(&conn, "alpha", "acme", "Alpha");
        insert_artifact(&conn, "bravo", "acme", "Bravo");
    }

    let script = [
        ("alpha", "acme", "a@x.test"),
        ("alpha", "acme", "a@x.test"),
        ("alpha", "acme", "b@x.test"),
        ("bravo", "acme", "a@x.test"),
        // Unknown artifact: the INSERT trips the composite foreign key and is swallowed.
        ("ghost", "acme", "a@x.test"),
        // Right artifact, wrong tenant: `ON CONFLICT(artifact_id, email)` matches the row written
        // above, so this takes the UPDATE branch, never touches `org`, and simply increments the
        // counter. Faithful to Node — the parity dump below proves both runtimes do it.
        ("alpha", "other", "a@x.test"),
    ];

    let node_results = run_node(
        node_fixture.path(),
        script
            .iter()
            .map(|(artifact, tenant, viewer)| {
                json!({ "kind": "record", "id": artifact, "org": tenant, "email": viewer })
            })
            .collect(),
    );
    assert!(
        node_results.iter().all(Value::is_null),
        "`views.record` never returns or throws (lib/views.js:40-46)"
    );

    let conn = rust_fixture.conn();
    for (artifact, tenant, viewer) in script {
        views::record(&conn, &id(artifact), &org(tenant), &email(viewer));
    }

    let dump = "SELECT artifact_id || '|' || org || '|' || email || '|' || count \
                FROM artifact_views ORDER BY artifact_id, email";
    assert_eq!(
        column(&conn, dump),
        column(&node_fixture.conn(), dump),
        "recorded rows diverged from the Node reference"
    );
    assert_eq!(
        column(&conn, dump),
        [
            "alpha|acme|a@x.test|3",
            "alpha|acme|b@x.test|1",
            "bravo|acme|a@x.test|1"
        ],
        "the unknown artifact wrote nothing; the wrong-tenant record folded into the existing row"
    );
}

#[test]
fn node_rejects_exactly_the_reaction_values_rust_rejects() {
    if !node_reference_available() {
        return;
    }
    let fixture = Fixture::new("parity-react-validate");

    let node = run_node(
        fixture.path(),
        vec![
            json!({ "kind": "parseReaction", "value": { "vote": 4 } }),
            json!({ "kind": "parseReaction", "value": { "vote": -2 } }),
            json!({ "kind": "parseReaction", "value": { "vote": 2 } }),
            json!({ "kind": "parseReaction", "value": { "vote": 1.5 } }),
            json!({ "kind": "parseReaction", "value": { "favorite": "yes" } }),
            json!({ "kind": "parseReaction", "value": { "vote": -1 } }),
            json!({ "kind": "parseReaction", "value": { "favorite": true, "vote": 0 } }),
        ],
    );

    for (index, value) in [4, -2, 2].into_iter().enumerate() {
        assert_eq!(
            node[index],
            json!({ "ok": false, "error": reactions::VOTE_VALUE_MESSAGE }),
            "Node's rejection of vote {value} must match the ported constant"
        );
    }
    assert_eq!(
        node[3],
        json!({ "ok": false, "error": reactions::VOTE_VALUE_MESSAGE }),
        "a non-integer vote is rejected by the same message"
    );
    assert_eq!(
        node[4],
        json!({ "ok": false, "error": reactions::FAVORITE_VALUE_MESSAGE })
    );
    assert_eq!(node[5], json!({ "ok": true, "value": { "vote": -1 } }));
    assert_eq!(
        node[6],
        json!({ "ok": true, "value": { "favorite": 1, "vote": 0 } })
    );

    // The Rust adapter rejects the same votes with the same message and writes nothing.
    let mut conn = fixture.conn();
    insert_artifact(&conn, "alpha", "acme", "Alpha");
    for vote in [4_i8, -2, 2] {
        let error = reactions::set(
            &mut conn,
            &email("a@x.test"),
            &id("alpha"),
            ReactionUpdate {
                favorite: None,
                vote: Some(vote),
            },
        )
        .expect_err("rejected vote");
        assert_eq!(error.to_string(), reactions::VOTE_VALUE_MESSAGE);
    }
    assert!(column(&conn, "SELECT email FROM reactions").is_empty());
}
