//! U10 behaviour suite: reactions, view analytics, and notification read watermarks.
//!
//! These run entirely on the Rust adapters over a real migrated SQLite file (U03's pool, so
//! `foreign_keys = ON` and the cascades are genuine). Cross-runtime agreement with `lib/` is
//! proved separately in `u10_node_parity.rs`.

use artifact_mcp::error::AppError;
use artifact_mcp::model::{
    ArtifactId, EmailAddress, OrgId, Reaction, ReactionUpdate, Sentiment, Timestamp, ViewCounts,
    Viewer,
};
use artifact_mcp::persistence::{notifications, reactions, views};
use rusqlite::Connection;

use crate::u10_support::{
    Fixture, column, insert_artifact, insert_feedback, insert_reaction, insert_view, scalar,
};

const PAST: &str = "2000-01-01 00:00:00";
const FUTURE: &str = "2999-01-01 00:00:00";

fn id(value: &str) -> ArtifactId {
    ArtifactId::from(value)
}

fn email(value: &str) -> EmailAddress {
    EmailAddress::from(value)
}

fn org(value: &str) -> OrgId {
    OrgId::from(value)
}

fn member(address: &str, tenant: &str) -> Viewer {
    Viewer {
        email: Some(email(address)),
        org: Some(org(tenant)),
        is_admin: false,
    }
}

fn admin(address: &str) -> Viewer {
    Viewer {
        email: Some(email(address)),
        org: Some(org("admin")),
        is_admin: true,
    }
}

// ---------------------------------------------------------------------------------------------
// Reactions
// ---------------------------------------------------------------------------------------------

#[test]
fn a_missing_reaction_row_reads_as_neutral() {
    let fixture = Fixture::new("react-default");
    let conn = fixture.conn();
    insert_artifact(&conn, "art-1", "acme", "One");

    assert_eq!(
        reactions::get(&conn, &email("viewer@acme.test"), &id("art-1")).expect("get"),
        Reaction {
            favorite: 0,
            vote: 0
        }
    );
}

#[test]
fn repeated_updates_replace_the_row_instead_of_duplicating_it() {
    let fixture = Fixture::new("react-upsert");
    let mut conn = fixture.conn();
    insert_artifact(&conn, "art-1", "acme", "One");
    let viewer = email("viewer@acme.test");

    for update in [
        ReactionUpdate {
            favorite: Some(true),
            vote: Some(1),
        },
        ReactionUpdate {
            favorite: Some(false),
            vote: Some(-1),
        },
        ReactionUpdate {
            favorite: Some(true),
            vote: Some(0),
        },
    ] {
        reactions::set(&mut conn, &viewer, &id("art-1"), update).expect("set reaction");
    }

    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM reactions WHERE email = 'viewer@acme.test' AND artifact_id = 'art-1'"
        ),
        1,
        "the upsert must keep exactly one row per (email, artifact_id)"
    );
    assert_eq!(
        reactions::get(&conn, &viewer, &id("art-1")).expect("get"),
        Reaction {
            favorite: 1,
            vote: 0
        }
    );
}

#[test]
fn a_partial_update_keeps_the_field_it_does_not_mention() {
    let fixture = Fixture::new("react-partial");
    let mut conn = fixture.conn();
    insert_artifact(&conn, "art-1", "acme", "One");
    let viewer = email("viewer@acme.test");

    let stored = reactions::set(
        &mut conn,
        &viewer,
        &id("art-1"),
        ReactionUpdate {
            favorite: Some(true),
            vote: Some(-1),
        },
    )
    .expect("seed reaction");
    assert_eq!(
        stored,
        Reaction {
            favorite: 1,
            vote: -1
        }
    );

    // Vote only: the favorite flag survives.
    let vote_only = reactions::set(
        &mut conn,
        &viewer,
        &id("art-1"),
        ReactionUpdate {
            favorite: None,
            vote: Some(1),
        },
    )
    .expect("vote-only update");
    assert_eq!(
        vote_only,
        Reaction {
            favorite: 1,
            vote: 1
        }
    );

    // Favorite only: the vote survives.
    let favorite_only = reactions::set(
        &mut conn,
        &viewer,
        &id("art-1"),
        ReactionUpdate {
            favorite: Some(false),
            vote: None,
        },
    )
    .expect("favorite-only update");
    assert_eq!(
        favorite_only,
        Reaction {
            favorite: 0,
            vote: 1
        }
    );

    // An empty update rewrites the same values, exactly as `lib/reactions.js:26-32` does.
    let noop = reactions::set(&mut conn, &viewer, &id("art-1"), ReactionUpdate::default())
        .expect("empty update");
    assert_eq!(noop, favorite_only);
}

#[test]
fn out_of_range_votes_are_rejected_with_nodes_message_and_nothing_is_written() {
    let fixture = Fixture::new("react-invalid");
    let mut conn = fixture.conn();
    insert_artifact(&conn, "art-1", "acme", "One");
    let viewer = email("viewer@acme.test");

    for vote in [-2_i8, 2, 4, 42, -128, 127] {
        let error = reactions::set(
            &mut conn,
            &viewer,
            &id("art-1"),
            ReactionUpdate {
                favorite: Some(true),
                vote: Some(vote),
            },
        )
        .expect_err("out-of-range vote must be rejected");
        assert_eq!(
            error,
            AppError::Validation("vote must be -1, 0, or 1.".to_owned()),
            "rejection message must match lib/contracts.js:68 byte for byte"
        );
    }

    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM reactions"),
        0,
        "a rejected update must not write a row"
    );

    for vote in [-1_i8, 0, 1] {
        reactions::set(
            &mut conn,
            &viewer,
            &id("art-1"),
            ReactionUpdate {
                favorite: None,
                vote: Some(vote),
            },
        )
        .expect("allowed vote");
    }
}

#[test]
fn the_schema_check_constraint_is_the_last_line_of_defence() {
    let fixture = Fixture::new("react-check");
    let conn = fixture.conn();
    insert_artifact(&conn, "art-1", "acme", "One");

    // v3 `reaction-integrity` (lib/migrations.js:120-121) rejects both illegal columns even when
    // the write bypasses the adapter entirely.
    for (favorite, vote) in [(0_i64, 2_i64), (0, -2), (2, 0), (-1, 0)] {
        conn.execute(
            "INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES ('v@acme.test', 'art-1', ?, ?)",
            (favorite, vote),
        )
        .expect_err("CHECK constraint must reject the row");
    }
}

#[test]
fn sentiment_aggregates_every_viewer_and_the_viewer_map_is_scoped_to_one() {
    let fixture = Fixture::new("react-aggregate");
    let conn = fixture.conn();
    insert_artifact(&conn, "art-1", "acme", "One");
    insert_artifact(&conn, "art-2", "acme", "Two");

    insert_reaction(&conn, "a@acme.test", "art-1", 1, 1);
    insert_reaction(&conn, "b@acme.test", "art-1", 1, 1);
    insert_reaction(&conn, "c@acme.test", "art-1", 0, -1);
    insert_reaction(&conn, "a@acme.test", "art-2", 0, 0);

    let sentiment = reactions::sentiment(&conn).expect("sentiment");
    assert_eq!(
        sentiment.get(&id("art-1")),
        Some(&Sentiment {
            up: 2,
            down: 1,
            favorites: 2
        })
    );
    assert_eq!(
        sentiment.get(&id("art-2")),
        Some(&Sentiment {
            up: 0,
            down: 0,
            favorites: 0
        })
    );

    let mine = reactions::for_viewer(&conn, &email("a@acme.test")).expect("for_viewer");
    assert_eq!(mine.len(), 2);
    assert_eq!(
        mine.get(&id("art-1")),
        Some(&Reaction {
            favorite: 1,
            vote: 1
        })
    );
    assert_eq!(
        mine.get(&id("art-2")),
        Some(&Reaction {
            favorite: 0,
            vote: 0
        })
    );
    assert!(
        reactions::for_viewer(&conn, &email("nobody@acme.test"))
            .expect("for_viewer")
            .is_empty()
    );
}

#[test]
fn deleting_an_artifact_cascades_its_reactions() {
    let fixture = Fixture::new("react-cascade");
    let conn = fixture.conn();
    insert_artifact(&conn, "art-1", "acme", "One");
    insert_artifact(&conn, "art-2", "acme", "Two");
    insert_reaction(&conn, "a@acme.test", "art-1", 1, 1);
    insert_reaction(&conn, "b@acme.test", "art-1", 0, -1);
    insert_reaction(&conn, "a@acme.test", "art-2", 1, 0);

    conn.execute("DELETE FROM artifacts WHERE id = 'art-1'", [])
        .expect("delete artifact");

    assert_eq!(
        column(
            &conn,
            "SELECT artifact_id FROM reactions ORDER BY artifact_id"
        ),
        ["art-2"],
        "ON DELETE CASCADE must remove every reaction for the deleted artifact"
    );
    assert_eq!(
        reactions::sentiment(&conn).expect("sentiment").len(),
        1,
        "the aggregate must forget the deleted artifact"
    );
}

// ---------------------------------------------------------------------------------------------
// View analytics
// ---------------------------------------------------------------------------------------------

#[test]
fn a_repeat_view_increments_the_total_without_adding_a_viewer() {
    let fixture = Fixture::new("views-repeat");
    let conn = fixture.conn();
    insert_artifact(&conn, "one", "acme", "One");

    views::record(
        &conn,
        &id("one"),
        &org("acme"),
        &email("viewer@example.test"),
    );
    conn.execute(
        "UPDATE artifact_views SET last_viewed_at = '2000-01-01 00:00:00' WHERE artifact_id = 'one'",
        [],
    )
    .expect("age the row");
    views::record(
        &conn,
        &id("one"),
        &org("acme"),
        &email("viewer@example.test"),
    );

    let counts = views::counts_for(&conn, &id("one")).expect("counts");
    assert_eq!(counts.views, 2);
    assert_eq!(counts.unique_viewers, 1);

    let viewers = views::viewers_for(&conn, &id("one")).expect("viewers");
    assert_eq!(viewers.len(), 1);
    assert_eq!(viewers[0].count, 2);
    assert_ne!(
        viewers[0].last_viewed_at,
        Timestamp(PAST.to_owned()),
        "the conflict branch must refresh last_viewed_at"
    );
    assert_eq!(
        counts.last_viewed_at,
        Some(viewers[0].last_viewed_at.clone())
    );
}

#[test]
fn an_unviewed_artifact_still_reports_a_zeroed_aggregate() {
    let fixture = Fixture::new("views-empty");
    let conn = fixture.conn();
    insert_artifact(&conn, "one", "acme", "One");

    assert_eq!(
        views::counts_for(&conn, &id("one")).expect("counts"),
        ViewCounts {
            views: 0,
            unique_viewers: 0,
            last_viewed_at: None
        }
    );
    assert!(
        views::viewers_for(&conn, &id("one"))
            .expect("viewers")
            .is_empty()
    );
    assert_eq!(
        views::counts_for(&conn, &id("never-existed")).expect("counts"),
        ViewCounts::default(),
        "the aggregate has no GROUP BY, so it always yields one row"
    );
}

#[test]
fn viewers_are_ordered_newest_visit_first() {
    let fixture = Fixture::new("views-order");
    let conn = fixture.conn();
    insert_artifact(&conn, "two", "acme", "Two");
    insert_view(
        &conn,
        "two",
        "acme",
        "first@example.test",
        2,
        "2026-01-01 00:00:00",
        "2026-01-01 09:00:00",
    );
    insert_view(
        &conn,
        "two",
        "acme",
        "last@example.test",
        1,
        "2026-01-02 00:00:00",
        "2026-01-02 09:00:00",
    );
    insert_view(
        &conn,
        "two",
        "acme",
        "middle@example.test",
        5,
        "2026-01-01 12:00:00",
        "2026-01-01 18:00:00",
    );

    let viewers = views::viewers_for(&conn, &id("two")).expect("viewers");
    assert_eq!(
        viewers
            .iter()
            .map(|viewer| viewer.email.0.as_str())
            .collect::<Vec<_>>(),
        [
            "last@example.test",
            "middle@example.test",
            "first@example.test"
        ],
        "ORDER BY last_viewed_at DESC is observable through artifact_stats"
    );
    assert_eq!(
        views::counts_for(&conn, &id("two")).expect("counts"),
        ViewCounts {
            views: 8,
            unique_viewers: 3,
            last_viewed_at: Some(Timestamp("2026-01-02 09:00:00".to_owned()))
        }
    );
}

#[test]
fn org_counts_only_cover_the_requested_tenant() {
    let fixture = Fixture::new("views-org");
    let conn = fixture.conn();
    insert_artifact(&conn, "four", "acme", "Four");
    insert_artifact(&conn, "five", "other", "Five");
    insert_view(&conn, "four", "acme", "a@x.test", 2, PAST, PAST);
    insert_view(&conn, "five", "other", "b@x.test", 1, PAST, PAST);

    let acme = views::counts_for_org(&conn, &org("acme")).expect("org counts");
    assert_eq!(acme.keys().collect::<Vec<_>>(), [&id("four")]);
    assert_eq!(acme[&id("four")].views, 2);
    assert_eq!(acme[&id("four")].unique_viewers, 1);
    assert_eq!(
        acme[&id("four")].last_viewed_at,
        None,
        "the org projection selects no timestamp (lib/views.js:21)"
    );

    let other = views::counts_for_org(&conn, &org("other")).expect("org counts");
    assert_eq!(other.keys().collect::<Vec<_>>(), [&id("five")]);
    assert!(
        views::counts_for_org(&conn, &org("ghost"))
            .expect("org counts")
            .is_empty()
    );
}

#[test]
fn top_artifacts_order_by_views_then_recency_and_honour_the_limit() {
    let fixture = Fixture::new("views-top");
    let conn = fixture.conn();
    for (artifact, count, last) in [
        ("a", 5_i64, "2026-01-01 00:00:00"),
        ("b", 9, "2026-01-01 00:00:00"),
        ("c", 5, "2026-02-01 00:00:00"),
        ("d", 1, "2026-03-01 00:00:00"),
    ] {
        insert_artifact(&conn, artifact, "acme", artifact);
        insert_view(&conn, artifact, "acme", "v@x.test", count, PAST, last);
    }
    insert_artifact(&conn, "e", "other", "e");
    insert_view(&conn, "e", "other", "v@x.test", 99, PAST, FUTURE);

    let top = views::top_for_org(&conn, &org("acme"), views::DEFAULT_TOP_LIMIT).expect("top");
    assert_eq!(
        top.iter()
            .map(|row| row.artifact_id.0.as_str())
            .collect::<Vec<_>>(),
        ["b", "c", "a", "d"],
        "views DESC, then last_viewed_at DESC"
    );
    assert_eq!(top[0].views, 9);
    assert_eq!(top[0].unique_viewers, 1);
    assert_eq!(top[0].title, "b");

    let limited = views::top_for_org(&conn, &org("acme"), 2).expect("top");
    assert_eq!(
        limited
            .iter()
            .map(|row| row.artifact_id.0.as_str())
            .collect::<Vec<_>>(),
        ["b", "c"]
    );

    // `Math.max(1, Number(limit) || 10)`: zero is falsy and becomes the default, not an empty page.
    assert_eq!(
        views::top_for_org(&conn, &org("acme"), 0)
            .expect("top")
            .len(),
        4
    );
}

#[test]
fn the_gallery_projection_merges_every_org_and_survives_a_broken_read() {
    let fixture = Fixture::new("views-gallery");
    let conn = fixture.conn();
    insert_artifact(&conn, "alpha", "acme", "Alpha");
    insert_artifact(&conn, "bravo", "beta", "Bravo");
    insert_view(&conn, "alpha", "acme", "v@x.test", 3, PAST, PAST);
    insert_view(&conn, "bravo", "beta", "v@x.test", 5, PAST, PAST);

    let tenants = [org("acme"), org("beta")];
    let member_view = views::gallery_analytics(&conn, &tenants[..1], false);
    assert_eq!(
        member_view.view_counts.keys().collect::<Vec<_>>(),
        [&id("alpha")]
    );
    assert!(
        member_view.top_viewed.is_empty(),
        "the most-viewed list is admin-only"
    );

    let admin_view = views::gallery_analytics(&conn, &tenants, true);
    assert_eq!(
        admin_view.view_counts.keys().collect::<Vec<_>>(),
        [&id("alpha"), &id("bravo")],
        "counts from every visible org are merged into one map"
    );
    assert_eq!(
        admin_view.top_viewed.keys().collect::<Vec<_>>(),
        [&org("acme"), &org("beta")]
    );

    // The gallery must still render when the analytics table is unusable.
    conn.execute("DROP TABLE artifact_views", [])
        .expect("drop analytics table");
    assert_eq!(
        views::gallery_analytics(&conn, &tenants, true),
        views::GalleryAnalytics::default()
    );
}

#[test]
fn deleting_an_artifact_cascades_its_view_analytics() {
    let fixture = Fixture::new("views-cascade");
    let conn = fixture.conn();
    insert_artifact(&conn, "three", "acme", "Three");
    insert_artifact(&conn, "keep", "acme", "Keep");
    views::record(&conn, &id("three"), &org("acme"), &email("v@x.test"));
    views::record(&conn, &id("keep"), &org("acme"), &email("v@x.test"));

    conn.execute("DELETE FROM artifacts WHERE id = 'three'", [])
        .expect("delete artifact");

    assert_eq!(
        views::counts_for(&conn, &id("three")).expect("counts"),
        ViewCounts::default()
    );
    assert_eq!(
        column(
            &conn,
            "SELECT artifact_id FROM artifact_views ORDER BY artifact_id"
        ),
        ["keep"]
    );
}

#[test]
fn recording_a_view_can_never_fail_the_request() {
    let fixture = Fixture::new("views-besteffort");
    let conn = fixture.conn();
    insert_artifact(&conn, "one", "acme", "One");

    // 1. Unknown artifact: the composite foreign key rejects the row.
    views::record(&conn, &id("ghost"), &org("acme"), &email("v@x.test"));
    // 2. Right artifact, wrong tenant: the (id, org) pair does not exist either.
    views::record(&conn, &id("one"), &org("other"), &email("v@x.test"));
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM artifact_views"),
        0
    );
    assert!(
        views::record_strict(&conn, &id("ghost"), &org("acme"), &email("v@x.test")).is_err(),
        "the strict variant still surfaces the driver error"
    );

    // 3. The analytics table itself is gone: reads and writes both degrade quietly.
    conn.execute("DROP TABLE artifact_views", [])
        .expect("drop analytics table");
    views::record(&conn, &id("one"), &org("acme"), &email("v@x.test"));

    let analytics = views::shell_analytics(&conn, &id("one"), true);
    assert_eq!(
        analytics.counts, None,
        "a failed aggregate read renders the shell without counts, as lib/app.js:503-509 does"
    );
    assert_eq!(analytics.viewers, None);
    assert!(matches!(
        views::counts_for(&conn, &id("one")),
        Err(AppError::Internal)
    ));
}

#[tokio::test]
async fn the_pooled_recorder_swallows_failures_too() {
    let fixture = Fixture::new("views-pooled");
    {
        let conn = fixture.conn();
        insert_artifact(&conn, "one", "acme", "One");
    }

    views::record_pooled(fixture.pool(), id("one"), org("acme"), email("v@x.test"))
        .await
        .expect("recording is infallible");
    assert_eq!(
        views::counts_for_pooled(fixture.pool(), id("one"))
            .await
            .expect("counts")
            .views,
        1
    );

    fixture
        .conn()
        .execute("DROP TABLE artifact_views", [])
        .expect("drop analytics table");

    views::record_pooled(fixture.pool(), id("one"), org("acme"), email("v@x.test"))
        .await
        .expect("a broken analytics table must not fail the caller");
    assert_eq!(
        views::shell_analytics_pooled(fixture.pool(), id("one"), true).await,
        views::ShellAnalytics::default()
    );
}

#[test]
fn only_the_shell_render_of_an_identified_member_is_attributed() {
    let fixture = Fixture::new("views-attribution");
    let conn = fixture.conn();
    insert_artifact(&conn, "one", "acme", "One");

    // Requests that deliberately do not count: raw HTML, bundle subresources, thumbnails. They
    // never reach this module at all, so the analytics table stays empty.
    raw_request(&conn, &id("one"));
    raw_request(&conn, &id("one"));
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM artifact_views"),
        0
    );

    // Admins browse without inflating a tenant's analytics.
    let analytics = shell_render(&conn, &id("one"), &org("acme"), &admin("root@example.test"));
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM artifact_views"),
        0
    );
    assert_eq!(
        analytics.counts,
        Some(ViewCounts::default()),
        "the admin still reads the aggregate"
    );
    assert_eq!(
        analytics.viewers,
        Some(Vec::new()),
        "and the admin-only viewer list"
    );

    // An unidentified viewer has no attribution key.
    shell_render(
        &conn,
        &id("one"),
        &org("acme"),
        &Viewer {
            email: None,
            org: Some(org("acme")),
            is_admin: false,
        },
    );
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM artifact_views"),
        0
    );

    // A signed-in member counts exactly once per shell render.
    let member_analytics = shell_render(
        &conn,
        &id("one"),
        &org("acme"),
        &member("m@acme.test", "acme"),
    );
    assert_eq!(member_analytics.counts.map(|counts| counts.views), Some(1));
    assert_eq!(
        member_analytics.viewers, None,
        "the viewer list is admin-only"
    );
    raw_request(&conn, &id("one"));
    shell_render(
        &conn,
        &id("one"),
        &org("acme"),
        &member("m@acme.test", "acme"),
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT count FROM artifact_views WHERE email = 'm@acme.test'"
        ),
        2,
        "two shell renders, two counted views; the raw fetch in between counts for nothing"
    );
}

/// The shell route's analytics contract (`lib/app.js:491-510`): record first when the viewer
/// qualifies, then read the projections best-effort.
fn shell_render(
    conn: &Connection,
    artifact: &ArtifactId,
    tenant: &OrgId,
    viewer: &Viewer,
) -> views::ShellAnalytics {
    if views::should_record(viewer)
        && let Some(address) = viewer.email.as_ref()
    {
        views::record(conn, artifact, tenant, address);
    }
    views::shell_analytics(conn, artifact, viewer.is_admin)
}

/// A raw/thumbnail/bundle-subresource request. Analytics are deliberately absent from this path
/// so an artifact embedding N iframes is still a single attributed view.
fn raw_request(_conn: &Connection, _artifact: &ArtifactId) {}

// ---------------------------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------------------------

/// Two orgs, one comment each, mirroring `test/notifications.test.js:20-36`.
fn seed_notifications(conn: &Connection) {
    insert_artifact(conn, "artifact-a", "acme", "Acme report");
    insert_artifact(conn, "artifact-b", "beta", "Beta report");
    insert_feedback(
        conn,
        "feedback-a",
        "artifact-a",
        "acme",
        "author@acme.test",
        "Acme note",
        "2026-07-14 10:00:00",
    );
    insert_feedback(
        conn,
        "feedback-b",
        "artifact-b",
        "beta",
        "author@beta.test",
        "Beta note",
        "2026-07-14 11:00:00",
    );
}

#[test]
fn member_listings_are_tenant_scoped_while_admins_see_every_org() {
    let fixture = Fixture::new("notify-scope");
    let conn = fixture.conn();
    seed_notifications(&conn);

    let member_rows = notifications::recent_for_viewer(
        &conn,
        &member("viewer@acme.test", "acme"),
        notifications::DEFAULT_LIMIT,
    )
    .expect("member notifications");
    assert_eq!(
        member_rows
            .iter()
            .map(|row| row.id.0.as_str())
            .collect::<Vec<_>>(),
        ["feedback-a"]
    );
    assert_eq!(member_rows[0].org, org("acme"));
    assert_eq!(member_rows[0].artifact_title, "Acme report");
    assert_eq!(member_rows[0].artifact_id, id("artifact-a"));
    assert!(!member_rows[0].resolved);
    assert!(!member_rows[0].has_anchor);
    assert!(member_rows[0].unread);
    assert_eq!(member_rows[0].parent_id, None);

    let admin_rows = notifications::recent_for_viewer(
        &conn,
        &admin("admin@example.test"),
        notifications::DEFAULT_LIMIT,
    )
    .expect("admin notifications");
    assert_eq!(
        admin_rows
            .iter()
            .map(|row| row.id.0.as_str())
            .collect::<Vec<_>>(),
        ["feedback-b", "feedback-a"],
        "newest first, across every tenant"
    );
}

#[test]
fn a_viewers_own_feedback_is_never_a_notification() {
    let fixture = Fixture::new("notify-self");
    let conn = fixture.conn();
    seed_notifications(&conn);
    insert_feedback(
        &conn,
        "feedback-self",
        "artifact-a",
        "acme",
        "viewer@acme.test",
        "My own note",
        "2026-07-15 10:00:00",
    );

    let viewer = member("viewer@acme.test", "acme");
    let rows = notifications::recent_for_viewer(&conn, &viewer, notifications::DEFAULT_LIMIT)
        .expect("notifications");
    assert!(rows.iter().all(|row| row.id.0 != "feedback-self"));
    assert_eq!(
        notifications::unread_count(&conn, &viewer).expect("unread"),
        1,
        "self-authored feedback is excluded from the badge too"
    );
}

#[test]
fn ordering_breaks_same_second_ties_on_the_feedback_id() {
    let fixture = Fixture::new("notify-order");
    let conn = fixture.conn();
    insert_artifact(&conn, "artifact-a", "acme", "Acme report");
    for suffix in ["a", "b", "c"] {
        insert_feedback(
            &conn,
            &format!("feedback-{suffix}"),
            "artifact-a",
            "acme",
            "author@acme.test",
            "note",
            "2026-07-14 10:00:00",
        );
    }
    insert_feedback(
        &conn,
        "feedback-newer",
        "artifact-a",
        "acme",
        "author@acme.test",
        "newer",
        "2026-07-14 11:00:00",
    );

    let rows = notifications::recent_for_viewer(
        &conn,
        &member("viewer@acme.test", "acme"),
        notifications::DEFAULT_LIMIT,
    )
    .expect("notifications");
    assert_eq!(
        rows.iter().map(|row| row.id.0.as_str()).collect::<Vec<_>>(),
        ["feedback-newer", "feedback-c", "feedback-b", "feedback-a"],
        "ORDER BY f.created_at DESC, f.id DESC"
    );
}

#[test]
fn the_limit_is_clamped_between_one_and_one_hundred() {
    let fixture = Fixture::new("notify-limit");
    let conn = fixture.conn();
    insert_artifact(&conn, "artifact-a", "acme", "Acme report");
    for index in 0..120 {
        insert_feedback(
            &conn,
            &format!("feedback-{index:03}"),
            "artifact-a",
            "acme",
            "author@acme.test",
            "note",
            "2026-07-14 10:00:00",
        );
    }

    let viewer = member("viewer@acme.test", "acme");
    let count = |limit: usize| {
        notifications::recent_for_viewer(&conn, &viewer, limit)
            .expect("notifications")
            .len()
    };
    assert_eq!(count(0), 30, "a falsy limit falls back to the default");
    assert_eq!(count(1), 1);
    assert_eq!(count(30), 30);
    assert_eq!(count(100), 100);
    assert_eq!(count(101), 100);
    assert_eq!(count(usize::MAX), 100);
}

#[test]
fn the_watermark_matrix_controls_unread_state() {
    let fixture = Fixture::new("notify-watermark");
    let conn = fixture.conn();
    seed_notifications(&conn);
    let viewer = member("viewer@acme.test", "acme");
    let viewer_email = email("viewer@acme.test");

    // 1. No watermark at all: everything above the epoch is unread.
    assert_eq!(
        notifications::watermark(&conn, &viewer_email).expect("watermark"),
        None
    );
    assert_eq!(
        notifications::unread_count(&conn, &viewer).expect("unread"),
        1
    );
    assert!(
        notifications::recent_for_viewer(&conn, &viewer, notifications::DEFAULT_LIMIT)
            .expect("notifications")[0]
            .unread
    );

    // 2. Marking seen clears the badge and flips the projection's unread flag.
    notifications::mark_seen(&conn, &viewer_email).expect("mark seen");
    assert!(
        notifications::watermark(&conn, &viewer_email)
            .expect("watermark")
            .is_some()
    );
    assert_eq!(
        notifications::unread_count(&conn, &viewer).expect("unread"),
        0
    );
    assert!(
        !notifications::recent_for_viewer(&conn, &viewer, notifications::DEFAULT_LIMIT)
            .expect("notifications")[0]
            .unread
    );

    // 3. Feedback created after the watermark is unread again; older feedback stays seen.
    insert_feedback(
        &conn,
        "feedback-future",
        "artifact-a",
        "acme",
        "author@acme.test",
        "later",
        FUTURE,
    );
    assert_eq!(
        notifications::unread_count(&conn, &viewer).expect("unread"),
        1
    );
    let rows = notifications::recent_for_viewer(&conn, &viewer, notifications::DEFAULT_LIMIT)
        .expect("notifications");
    assert_eq!(
        rows.iter()
            .map(|row| (row.id.0.as_str(), row.unread))
            .collect::<Vec<_>>(),
        [("feedback-future", true), ("feedback-a", false)]
    );

    // 4. One watermark per viewer, and it is scoped to that viewer only.
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM notification_reads"),
        1
    );
    assert_eq!(
        notifications::unread_count(&conn, &member("other@acme.test", "acme")).expect("unread"),
        2,
        "another member has no watermark yet"
    );
}

#[test]
fn the_watermark_never_moves_backwards() {
    let fixture = Fixture::new("notify-monotonic");
    let conn = fixture.conn();
    seed_notifications(&conn);
    let viewer_email = email("viewer@acme.test");

    notifications::mark_seen(&conn, &viewer_email).expect("mark seen");
    conn.execute(
        "UPDATE notification_reads SET seen_at = ?1 WHERE viewer_email = ?2",
        (FUTURE, &viewer_email.0),
    )
    .expect("advance the watermark");

    // A second mark-seen with an older `datetime('now')` must not rewind the watermark.
    notifications::mark_seen(&conn, &viewer_email).expect("mark seen again");
    assert_eq!(
        notifications::watermark(&conn, &viewer_email).expect("watermark"),
        Some(Timestamp(FUTURE.to_owned()))
    );
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM notification_reads"),
        1,
        "mark-seen upserts, it does not append"
    );
}

#[test]
fn an_unidentified_viewer_degrades_to_an_empty_identity() {
    let fixture = Fixture::new("notify-anonymous");
    let conn = fixture.conn();
    seed_notifications(&conn);

    // Member with no org matches no tenant, so the projection is empty rather than an error.
    let anonymous = Viewer::default();
    assert!(
        notifications::recent_for_viewer(&conn, &anonymous, notifications::DEFAULT_LIMIT)
            .expect("notifications")
            .is_empty()
    );
    assert_eq!(
        notifications::unread_count(&conn, &anonymous).expect("unread"),
        0
    );

    // An admin with no email authored nothing, so every row is "not mine".
    let headless_admin = Viewer {
        email: None,
        org: None,
        is_admin: true,
    };
    assert_eq!(
        notifications::unread_count(&conn, &headless_admin).expect("unread"),
        2
    );
}

#[tokio::test]
async fn the_pooled_watermark_path_runs_on_the_blocking_pool() {
    let fixture = Fixture::new("notify-pooled");
    {
        let conn = fixture.conn();
        seed_notifications(&conn);
    }
    let viewer = member("viewer@acme.test", "acme");

    assert_eq!(
        notifications::unread_count_pooled(fixture.pool(), viewer.clone())
            .await
            .expect("unread"),
        1
    );
    notifications::mark_seen_pooled(fixture.pool(), email("viewer@acme.test"))
        .await
        .expect("mark seen");
    assert_eq!(
        notifications::unread_count_pooled(fixture.pool(), viewer.clone())
            .await
            .expect("unread"),
        0
    );
    let rows = notifications::recent_for_viewer_pooled(
        fixture.pool(),
        viewer,
        notifications::DEFAULT_LIMIT,
    )
    .await
    .expect("notifications");
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].unread);
}

#[tokio::test]
async fn the_pooled_reaction_path_runs_on_the_blocking_pool() {
    let fixture = Fixture::new("react-pooled");
    {
        let conn = fixture.conn();
        insert_artifact(&conn, "art-1", "acme", "One");
    }

    let stored = reactions::set_pooled(
        fixture.pool(),
        email("viewer@acme.test"),
        id("art-1"),
        ReactionUpdate {
            favorite: Some(true),
            vote: Some(-1),
        },
    )
    .await
    .expect("set reaction");
    assert_eq!(
        stored,
        Reaction {
            favorite: 1,
            vote: -1
        }
    );
    assert_eq!(
        reactions::get_pooled(fixture.pool(), email("viewer@acme.test"), id("art-1"))
            .await
            .expect("get reaction"),
        stored
    );
    assert_eq!(
        reactions::for_viewer_pooled(fixture.pool(), email("viewer@acme.test"))
            .await
            .expect("viewer reactions")
            .len(),
        1
    );
    assert_eq!(
        reactions::sentiment_pooled(fixture.pool())
            .await
            .expect("sentiment")
            .get(&id("art-1")),
        Some(&Sentiment {
            up: 0,
            down: 1,
            favorites: 1
        })
    );

    let rejected = reactions::set_pooled(
        fixture.pool(),
        email("viewer@acme.test"),
        id("art-1"),
        ReactionUpdate {
            favorite: None,
            vote: Some(7),
        },
    )
    .await
    .expect_err("out-of-range vote");
    assert_eq!(
        rejected,
        AppError::Validation("vote must be -1, 0, or 1.".to_owned())
    );
}
