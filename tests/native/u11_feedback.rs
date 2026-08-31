//! U11 — viewer feedback: threads, anchors, state transitions, and listing order.

use artifact_mcp::config::{FEEDBACK_ID_ALPHABET, FEEDBACK_ID_LENGTH, IdSource, NanoIdSource};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{
    ArtifactId, EmailAddress, Feedback, FeedbackAnchor, FeedbackAnchorV2, FeedbackId,
    FeedbackRef, OrgId, SubmitFeedback,
};
use artifact_mcp::persistence::feedback::{
    self, ANCHOR_BOX_BOUNDS_MESSAGE, ANCHOR_BOX_PAIR_MESSAGE, ANCHOR_BOX_POSITIVE_MESSAGE,
    ANCHOR_BOX_RANGE_MESSAGE, ANCHOR_PAGE_MISSING_MESSAGE, ANCHOR_PAGE_NOT_A_FILE_MESSAGE,
    ANCHOR_PAGE_NOT_BUNDLE_MESSAGE, ANCHOR_PAGE_REQUIRED_MESSAGE, ANCHOR_PAGE_TRAVERSAL_MESSAGE,
    ANCHOR_PAGE_UNANCHORED_MESSAGE, ANCHOR_POINT_MESSAGE, EMPTY_BODY_MESSAGE, FORBIDDEN_MESSAGE,
    NOT_FOUND_MESSAGE, NewFeedback, PARENT_NOT_FOUND_MESSAGE, PARENT_NOT_TOP_LEVEL_MESSAGE,
    PARENT_OTHER_ARTIFACT_MESSAGE, too_long_message,
};
use rusqlite::params;

use crate::u11_support::{Fixture, client, org};

const ORG: &str = "acme";
const CLIENT: &str = "key-acme";
const MAX_BODY: u64 = 4_000;
const VIEWER: &str = "viewer@example.com";

fn submission(body: &str) -> SubmitFeedback {
    SubmitFeedback {
        viewer_email: EmailAddress::from(VIEWER),
        body: body.to_owned(),
        parent_id: None,
        anchor: None,
        anchor_path: None,
        anchor_page: None,
        anchor_v2: None,
    }
}

fn new_feedback_in<'a>(
    artifact: &'a ArtifactId,
    org: &'a OrgId,
    submission: &'a SubmitFeedback,
) -> NewFeedback<'a> {
    NewFeedback {
        artifact_id: artifact,
        org,
        artifact_revision: 1,
        submission,
        anchor_page: None,
        max_body: MAX_BODY,
    }
}

fn point(x: f64, y: f64) -> FeedbackAnchor {
    FeedbackAnchor {
        x,
        y,
        w: None,
        h: None,
        approx: false,
    }
}

fn boxed(x: f64, y: f64, w: f64, h: f64) -> FeedbackAnchor {
    FeedbackAnchor {
        x,
        y,
        w: Some(w),
        h: Some(h),
        approx: true,
    }
}

fn bodies(rows: Vec<Feedback>) -> Vec<String> {
    rows.into_iter().map(|row| row.body).collect()
}

// ---------------------------------------------------------------------------
// Body
// ---------------------------------------------------------------------------

#[test]
fn feedback_ids_are_16_symbols_of_the_frozen_lowercase_alphabet() {
    // Unlike artifact ids, this alphabet keeps `l` and `o` [lib/feedback.js:10].
    assert_eq!(FEEDBACK_ID_LENGTH, 16);
    assert_eq!(FEEDBACK_ID_ALPHABET, "0123456789abcdefghijklmnopqrstuvwxyz");
    let ids = NanoIdSource::default();
    for _ in 0..500 {
        let id = ids.feedback_id().expect("mint a feedback id");
        assert_eq!(id.0.chars().count(), FEEDBACK_ID_LENGTH);
        assert!(
            id.0.chars()
                .all(|symbol| FEEDBACK_ID_ALPHABET.contains(symbol))
        );
    }
}

#[test]
fn body_is_trimmed_and_bounded_by_feedback_max_body() {
    let fixture = Fixture::new("u11-body");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("body00000001", ORG, CLIENT);
    let org_id = org(ORG);

    let add = |submission: &SubmitFeedback, max_body: u64| {
        feedback::add(
            &conn,
            &fixture.ids,
            &NewFeedback {
                artifact_id: &artifact,
                org: &org_id,
                artifact_revision: 3,
                submission,
                anchor_page: None,
                max_body,
            },
        )
    };

    for empty in ["", "   ", "\t\r\n", "\u{a0}\u{feff}"] {
        assert_eq!(
            add(&submission(empty), MAX_BODY),
            Err(AppError::Validation(EMPTY_BODY_MESSAGE.to_owned())),
            "{empty:?} is an empty body"
        );
    }

    // The stored body is the trimmed one, and the revision is copied off the artifact.
    let stored = add(&submission("  hello  "), MAX_BODY).expect("add feedback");
    assert_eq!(stored.body, "hello");
    assert_eq!(stored.artifact_revision, 3);
    assert_eq!(stored.viewer_email, Some(EmailAddress::from(VIEWER)));
    assert_eq!(stored.parent_id, None);
    assert_eq!(stored.resolved_at, None);
    assert_eq!(stored.anchor_x, None);
    assert_eq!(stored.anchor_page, None);

    // Boundary: exactly the limit passes, one more fails, and the limit is measured *after*
    // trimming [lib/feedback.js:85-87].
    let at_limit = "x".repeat(usize::try_from(MAX_BODY).expect("fits"));
    assert!(add(&submission(&at_limit), MAX_BODY).is_ok());
    assert_eq!(
        add(&submission(&format!("{at_limit}x")), MAX_BODY),
        Err(AppError::Validation(too_long_message(MAX_BODY)))
    );
    assert!(
        add(&submission(&format!("  {at_limit}  ")), MAX_BODY).is_ok(),
        "surrounding whitespace does not count towards the limit"
    );

    // `String.prototype.length` counts UTF-16 code units, so an astral character costs two.
    let emoji = "\u{1f600}".repeat(3);
    assert_eq!(
        add(&submission(&emoji), 5),
        Err(AppError::Validation(too_long_message(5))),
        "three emoji are six JavaScript characters"
    );
    assert!(add(&submission(&emoji), 6).is_ok());
}

#[test]
fn a_body_over_the_schema_bound_is_a_validation_error_not_an_internal_one() {
    let fixture = Fixture::new("u11-check");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("checkbound01", ORG, CLIENT);
    let org_id = org(ORG);

    // `FEEDBACK_MAX_BODY` is an environment knob, but the `feedback` table carries its own
    // `CHECK (length(trim(body)) BETWEEN 1 AND 4000)` from migration 5. Configure the knob above
    // the schema bound and the insert is rejected by SQLite instead of by the length guard.
    // Node's route surfaces that thrown `SqliteError` as a 400 with SQLite's own message
    // [lib/app.js:599-601], so the port maps a constraint violation to `Validation`, not to the
    // 500 that a naive "any SQL error is infrastructure" mapping would produce.
    let body = submission(&"x".repeat(4_001));
    let error = feedback::add(
        &conn,
        &fixture.ids,
        &NewFeedback {
            max_body: 8_000,
            ..new_feedback_in(&artifact, &org_id, &body)
        },
    )
    .expect_err("the schema bound still applies");
    match error {
        AppError::Validation(message) => assert!(
            message.contains("CHECK constraint failed"),
            "unexpected message: {message}"
        ),
        other => panic!("expected a validation error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

#[test]
fn replies_are_one_level_deep_and_scoped_to_their_parents_artifact() {
    let fixture = Fixture::new("u11-threads");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("thread000001", ORG, CLIENT);
    let elsewhere = fixture.seed_artifact("thread000002", ORG, CLIENT);
    let org_id = org(ORG);

    let add = |artifact: &ArtifactId, submission: &SubmitFeedback| {
        feedback::add(
            &conn,
            &fixture.ids,
            &new_feedback_in(artifact, &org_id, submission),
        )
    };

    let parent = add(&artifact, &submission("top level")).expect("add parent");
    let foreign = add(&elsewhere, &submission("other artifact")).expect("add elsewhere");

    let reply_to = |parent: &FeedbackId| {
        let mut input = submission("a reply");
        input.parent_id = Some(parent.clone());
        input
    };

    let reply = add(&artifact, &reply_to(&parent.id)).expect("add reply");
    assert_eq!(reply.parent_id, Some(parent.id.clone()));

    assert_eq!(
        add(&artifact, &reply_to(&FeedbackId::from("nope"))),
        Err(AppError::Validation(PARENT_NOT_FOUND_MESSAGE.to_owned()))
    );
    assert_eq!(
        add(&artifact, &reply_to(&foreign.id)),
        Err(AppError::Validation(
            PARENT_OTHER_ARTIFACT_MESSAGE.to_owned()
        ))
    );
    assert_eq!(
        add(&artifact, &reply_to(&reply.id)),
        Err(AppError::Validation(
            PARENT_NOT_TOP_LEVEL_MESSAGE.to_owned()
        ))
    );
    // `parentId === ""` is "no parent", not "a parent named the empty string".
    let empty_parent = add(&artifact, &reply_to(&FeedbackId::from(""))).expect("add");
    assert_eq!(empty_parent.parent_id, None);

    // A reply silently drops any anchor and page it was given [lib/feedback.js:106-107].
    let mut anchored_reply = reply_to(&parent.id);
    anchored_reply.anchor = Some(point(0.5, 0.5));
    anchored_reply.anchor_path = Some("#heading".to_owned());
    let stored = feedback::add(
        &conn,
        &fixture.ids,
        &NewFeedback {
            anchor_page: Some("page.html"),
            ..new_feedback_in(&artifact, &org_id, &anchored_reply)
        },
    )
    .expect("add an anchored reply");
    assert_eq!(stored.anchor_x, None);
    assert_eq!(stored.anchor_y, None);
    assert_eq!(stored.anchor_path, None);
    assert_eq!(stored.anchor_page, None);
    assert!(!stored.anchor_approx);
}

#[test]
fn deleting_a_parent_cascades_to_its_replies_only() {
    let fixture = Fixture::new("u11-cascade");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("cascade00001", ORG, CLIENT);
    let org_id = org(ORG);
    let add = |submission: &SubmitFeedback| {
        feedback::add(
            &conn,
            &fixture.ids,
            &new_feedback_in(&artifact, &org_id, submission),
        )
        .expect("add feedback")
    };

    let parent = add(&submission("parent"));
    let survivor = add(&submission("unrelated top level"));
    let mut reply_body = submission("reply one");
    reply_body.parent_id = Some(parent.id.clone());
    let reply_one = add(&reply_body);
    reply_body.body = "reply two".to_owned();
    let reply_two = add(&reply_body);

    let scope = FeedbackRef {
        id: parent.id.clone(),
        artifact_id: artifact.clone(),
        org: org_id.clone(),
    };
    let mutation = feedback::delete_as_viewer(&conn, &scope, &EmailAddress::from(VIEWER), false)
        .expect("delete the thread");
    assert_eq!(mutation.id, parent.id);
    assert!(mutation.changed);

    // The self-referencing `ON DELETE CASCADE` from migration 13 removes the whole thread — and
    // only works because `foreign_keys = ON` is pinned on every pooled connection.
    for gone in [&parent.id, &reply_one.id, &reply_two.id] {
        assert_eq!(
            feedback::get(&conn, gone),
            Ok(None),
            "{gone} should be gone"
        );
    }
    assert!(
        feedback::get(&conn, &survivor.id).expect("read").is_some(),
        "an unrelated top-level item must survive"
    );
}

// ---------------------------------------------------------------------------
// Anchors
// ---------------------------------------------------------------------------

#[test]
fn anchor_validation_matrix() {
    let out_of_range = AppError::Validation(ANCHOR_POINT_MESSAGE.to_owned());
    for anchor in [
        point(-0.000_1, 0.5),
        point(1.000_1, 0.5),
        point(0.5, -0.000_1),
        point(0.5, 1.000_1),
        point(f64::NAN, 0.5),
        point(0.5, f64::NAN),
        point(f64::INFINITY, 0.5),
        point(f64::NEG_INFINITY, 0.5),
    ] {
        assert_eq!(
            feedback::normalize_anchor(Some(&anchor), None),
            Err(out_of_range.clone()),
            "{anchor:?} is outside the unit square"
        );
    }
    // The interval is closed at both ends.
    for anchor in [point(0.0, 0.0), point(1.0, 1.0), point(0.0, 1.0)] {
        assert!(feedback::normalize_anchor(Some(&anchor), None).is_ok());
    }

    // w and h are all-or-nothing.
    let half_box = FeedbackAnchor {
        x: 0.1,
        y: 0.1,
        w: Some(0.2),
        h: None,
        approx: false,
    };
    assert_eq!(
        feedback::normalize_anchor(Some(&half_box), None),
        Err(AppError::Validation(ANCHOR_BOX_PAIR_MESSAGE.to_owned()))
    );
    let other_half = FeedbackAnchor {
        w: None,
        h: Some(0.2),
        ..half_box
    };
    assert_eq!(
        feedback::normalize_anchor(Some(&other_half), None),
        Err(AppError::Validation(ANCHOR_BOX_PAIR_MESSAGE.to_owned()))
    );

    // Out-of-range box dimensions, checked before the positivity rule.
    for anchor in [
        boxed(0.1, 0.1, 1.5, 0.2),
        boxed(0.1, 0.1, 0.2, 1.5),
        boxed(0.1, 0.1, -0.5, 0.2),
        boxed(0.1, 0.1, f64::NAN, 0.2),
    ] {
        assert_eq!(
            feedback::normalize_anchor(Some(&anchor), None),
            Err(AppError::Validation(ANCHOR_BOX_RANGE_MESSAGE.to_owned())),
            "{anchor:?} is not a unit-interval box"
        );
    }
    // A zero-area box is in range but still rejected.
    for anchor in [boxed(0.1, 0.1, 0.0, 0.2), boxed(0.1, 0.1, 0.2, 0.0)] {
        assert_eq!(
            feedback::normalize_anchor(Some(&anchor), None),
            Err(AppError::Validation(ANCHOR_BOX_POSITIVE_MESSAGE.to_owned()))
        );
    }

    // An over-hanging box is trimmed to the edge rather than rejected [lib/feedback.js:28-31].
    let trimmed = feedback::normalize_anchor(Some(&boxed(0.8, 0.9, 0.5, 0.5)), None)
        .expect("an over-hanging box is trimmed");
    assert!((trimmed.anchor_w.expect("w") - 0.2).abs() < 1e-9);
    assert!((trimmed.anchor_h.expect("h") - 0.1).abs() < 1e-9);
    assert!(trimmed.anchor_approx);

    // …unless it starts on the far edge, where trimming leaves no area at all.
    for anchor in [boxed(1.0, 0.5, 0.2, 0.2), boxed(0.5, 1.0, 0.2, 0.2)] {
        assert_eq!(
            feedback::normalize_anchor(Some(&anchor), None),
            Err(AppError::Validation(ANCHOR_BOX_BOUNDS_MESSAGE.to_owned())),
            "{anchor:?} trims to zero area"
        );
    }

    // `String(anchor.path).slice(0, 512)`.
    let long_path = "s".repeat(600);
    let normalized = feedback::normalize_anchor(Some(&point(0.5, 0.5)), Some(&long_path))
        .expect("a long path is truncated, not rejected");
    assert_eq!(normalized.anchor_path.as_deref().map(str::len), Some(512));

    // An unanchored submission stores nothing and discards a stray path, exactly as
    // `normalizeAnchor(null)` does.
    let empty = feedback::normalize_anchor(None, Some("#ignored")).expect("no anchor");
    assert_eq!(empty.anchor_path, None);
    assert_eq!(empty.anchor_x, None);
    assert!(!empty.anchor_approx);
}

#[test]
fn structured_anchor_v2_persists_and_malformed_metadata_is_rejected() {
    let fixture = Fixture::new("u11-anchor-v2");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("anchorv20001", ORG, CLIENT);
    let org_id = org(ORG);
    let anchor = FeedbackAnchor {
        x: 0.25,
        y: 0.5,
        w: Some(0.25),
        h: Some(0.2),
        approx: false,
    };
    let mut input = submission("Structured");
    input.anchor = Some(anchor);
    input.anchor_path = Some("main:nth-child(1)".to_owned());
    input.anchor_v2 = Some(FeedbackAnchorV2 {
        version: Some(2.0),
        kind: Some("element".to_owned()),
        node_id: Some("revenue-table".to_owned()),
        quote: Some("Quarterly revenue".to_owned()),
        path_is_string: true,
        node_id_is_string_or_null: true,
        quote_is_string_or_null: true,
        approx_is_boolean_or_absent: true,
    });
    let stored = feedback::add(
        &conn,
        &fixture.ids,
        &new_feedback_in(&artifact, &org_id, &input),
    )
    .expect("persist v2 anchor");
    assert_eq!(stored.anchor_version, 2);
    assert_eq!(stored.anchor_kind.as_deref(), Some("element"));
    assert_eq!(stored.anchor_node_id.as_deref(), Some("revenue-table"));
    assert_eq!(stored.anchor_quote.as_deref(), Some("Quarterly revenue"));

    let mut malformed = input;
    malformed.anchor_v2.as_mut().expect("v2").version = Some(1.0);
    assert_eq!(
        feedback::add(
            &conn,
            &fixture.ids,
            &new_feedback_in(&artifact, &org_id, &malformed),
        ),
        Err(AppError::Validation(feedback::ANCHOR_VERSION_MESSAGE.to_owned()))
    );

    let mut malformed_approx = malformed;
    let v2 = malformed_approx.anchor_v2.as_mut().expect("v2");
    v2.version = Some(2.0);
    v2.approx_is_boolean_or_absent = false;
    assert_eq!(
        feedback::add(
            &conn,
            &fixture.ids,
            &new_feedback_in(&artifact, &org_id, &malformed_approx),
        ),
        Err(AppError::Validation(feedback::ANCHOR_APPROX_V2_MESSAGE.to_owned()))
    );
}

#[test]
fn anchor_page_matrix() {
    let pages = |candidate: &str| matches!(candidate, "index.html" | "docs/guide.html");
    let anchor = point(0.25, 0.75);

    // Unanchored: a page is meaningless, and an absent one is fine.
    assert_eq!(
        feedback::validate_anchor_page(true, None, Some("index.html"), &pages),
        Err(AppError::Validation(
            ANCHOR_PAGE_UNANCHORED_MESSAGE.to_owned()
        ))
    );
    assert_eq!(
        feedback::validate_anchor_page(true, None, None, &pages),
        Ok(None)
    );
    assert_eq!(
        feedback::validate_anchor_page(true, None, Some(""), &pages),
        Ok(None),
        "an empty string is 'absent', not 'supplied'"
    );

    // Anchored but not a bundle.
    assert_eq!(
        feedback::validate_anchor_page(false, Some(&anchor), Some("index.html"), &pages),
        Err(AppError::Validation(
            ANCHOR_PAGE_NOT_BUNDLE_MESSAGE.to_owned()
        ))
    );
    assert_eq!(
        feedback::validate_anchor_page(false, Some(&anchor), None, &pages),
        Ok(None)
    );

    // Anchored bundle feedback must name a page.
    for missing in [None, Some(""), Some("   "), Some("\u{feff}")] {
        assert_eq!(
            feedback::validate_anchor_page(true, Some(&anchor), missing, &pages),
            Err(AppError::Validation(
                ANCHOR_PAGE_REQUIRED_MESSAGE.to_owned()
            )),
            "{missing:?} does not name a page"
        );
    }

    // Traversal and absolute forms, including the backslash spellings.
    for hostile in [
        "/index.html",
        "//index.html",
        "../index.html",
        "docs/../../index.html",
        "..",
        "\\index.html",
        "docs\\..\\..\\index.html",
        "C:/index.html",
        "c:/index.html",
        "C:\\index.html",
    ] {
        assert_eq!(
            feedback::validate_anchor_page(true, Some(&anchor), Some(hostile), &pages),
            Err(AppError::Validation(
                ANCHOR_PAGE_TRAVERSAL_MESSAGE.to_owned()
            )),
            "{hostile:?} must be rejected as traversal"
        );
    }

    // Normalizes away to nothing at all.
    assert_eq!(
        feedback::validate_anchor_page(true, Some(&anchor), Some("."), &pages),
        Err(AppError::Validation(
            ANCHOR_PAGE_NOT_A_FILE_MESSAGE.to_owned()
        ))
    );
    assert_eq!(
        feedback::validate_anchor_page(true, Some(&anchor), Some("./."), &pages),
        Err(AppError::Validation(
            ANCHOR_PAGE_NOT_A_FILE_MESSAGE.to_owned()
        ))
    );
    // `path.posix.normalize` *keeps* a trailing slash, so `"././"` becomes `"./"` rather than
    // `"."` and falls through to the file lookup instead of the emptiness check.
    assert_eq!(
        feedback::validate_anchor_page(true, Some(&anchor), Some("././"), &pages),
        Err(AppError::Validation(ANCHOR_PAGE_MISSING_MESSAGE.to_owned()))
    );

    // Names a file that is not in the bundle, or is not HTML.
    for absent in ["missing.html", "docs/styles.css", "docs/guide.html/"] {
        assert_eq!(
            feedback::validate_anchor_page(true, Some(&anchor), Some(absent), &pages),
            Err(AppError::Validation(ANCHOR_PAGE_MISSING_MESSAGE.to_owned())),
            "{absent:?}"
        );
    }

    // Accepted, normalized forms.
    for (input, expected) in [
        ("index.html", "index.html"),
        ("  index.html  ", "index.html"),
        ("./index.html", "index.html"),
        ("docs/guide.html", "docs/guide.html"),
        ("docs//guide.html", "docs/guide.html"),
        ("docs/./guide.html", "docs/guide.html"),
        ("docs\\guide.html", "docs/guide.html"),
    ] {
        assert_eq!(
            feedback::validate_anchor_page(true, Some(&anchor), Some(input), &pages),
            Ok(Some(expected.to_owned())),
            "{input:?}"
        );
    }
}

#[test]
fn an_anchor_page_is_persisted_only_for_anchored_top_level_feedback() {
    let fixture = Fixture::new("u11-anchor-page");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("anchorpage01", ORG, CLIENT);
    let org_id = org(ORG);

    let mut anchored = submission("anchored");
    anchored.anchor = Some(boxed(0.1, 0.2, 0.3, 0.4));
    anchored.anchor_path = Some("#section-2".to_owned());
    let stored = feedback::add(
        &conn,
        &fixture.ids,
        &NewFeedback {
            anchor_page: Some("docs/guide.html"),
            ..new_feedback_in(&artifact, &org_id, &anchored)
        },
    )
    .expect("add anchored feedback");
    assert_eq!(stored.anchor_page.as_deref(), Some("docs/guide.html"));
    assert_eq!(stored.anchor_x, Some(0.1));
    assert_eq!(stored.anchor_y, Some(0.2));
    assert_eq!(stored.anchor_w, Some(0.3));
    assert_eq!(stored.anchor_h, Some(0.4));
    assert!(stored.anchor_approx);
    assert_eq!(stored.anchor_path.as_deref(), Some("#section-2"));

    // Without an anchor the page is dropped even when the caller supplies one — the store's own
    // guard, independent of the route's validation [lib/feedback.js:106].
    let unanchored = submission("unanchored");
    let stored = feedback::add(
        &conn,
        &fixture.ids,
        &NewFeedback {
            anchor_page: Some("docs/guide.html"),
            ..new_feedback_in(&artifact, &org_id, &unanchored)
        },
    )
    .expect("add unanchored feedback");
    assert_eq!(stored.anchor_page, None);
}

// ---------------------------------------------------------------------------
// State transitions
// ---------------------------------------------------------------------------

#[test]
fn resolve_and_reopen_are_one_way_transitions() {
    let fixture = Fixture::new("u11-transitions");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("resolve00001", ORG, CLIENT);
    let org_id = org(ORG);
    let body = submission("please fix");
    let item = feedback::add(
        &conn,
        &fixture.ids,
        &new_feedback_in(&artifact, &org_id, &body),
    )
    .expect("add feedback");

    assert_eq!(
        feedback::resolve_as_publisher(&conn, &item.id, "agent:key-acme"),
        Ok(true)
    );
    // A second resolve reports "no transition", which is what suppresses a duplicate webhook.
    assert_eq!(
        feedback::resolve_as_publisher(&conn, &item.id, "agent:key-acme"),
        Ok(false)
    );
    let resolved = feedback::get(&conn, &item.id)
        .expect("read")
        .expect("still there");
    assert_eq!(resolved.resolved_by.as_deref(), Some("agent:key-acme"));
    assert!(resolved.resolved_at.is_some());

    assert_eq!(feedback::reopen(&conn, &item.id), Ok(true));
    assert_eq!(
        feedback::reopen(&conn, &item.id),
        Ok(false),
        "reopening an open item is not a transition"
    );
    let reopened = feedback::get(&conn, &item.id)
        .expect("read")
        .expect("still there");
    assert_eq!(reopened.resolved_at, None);
    // The schema's `CHECK ((resolved_at IS NULL) = (resolved_by IS NULL))` means the resolver has
    // to be cleared too, not just the timestamp.
    assert_eq!(reopened.resolved_by, None);

    assert_eq!(
        feedback::reopen(&conn, &FeedbackId::from("does-not-exist")),
        Ok(false)
    );
    assert_eq!(
        feedback::resolve_as_publisher(&conn, &FeedbackId::from("does-not-exist"), "agent:x"),
        Ok(false)
    );
}

#[test]
fn viewer_mutations_check_artifact_scope_before_ownership() {
    let fixture = Fixture::new("u11-viewer-scope");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("viewer000001", ORG, CLIENT);
    let elsewhere = fixture.seed_artifact("viewer000002", ORG, CLIENT);
    let org_id = org(ORG);
    let body = submission("mine");
    let mine = feedback::add(
        &conn,
        &fixture.ids,
        &new_feedback_in(&artifact, &org_id, &body),
    )
    .expect("add");

    let scope = |artifact_id: &ArtifactId, org: &OrgId, id: &FeedbackId| FeedbackRef {
        id: id.clone(),
        artifact_id: artifact_id.clone(),
        org: org.clone(),
    };
    let viewer = EmailAddress::from(VIEWER);
    let stranger = EmailAddress::from("stranger@example.com");

    // Wrong artifact, wrong org, and a missing id all produce the *same* 404, so a feedback id is
    // not a cross-tenant existence oracle [lib/app.js:624-626].
    for wrong in [
        scope(&elsewhere, &org_id, &mine.id),
        scope(&artifact, &org("globex"), &mine.id),
        scope(&artifact, &org_id, &FeedbackId::from("missing")),
    ] {
        assert_eq!(
            feedback::resolve_as_viewer(&conn, &wrong, &viewer, false),
            Err(AppError::NotFound(NOT_FOUND_MESSAGE.to_owned()))
        );
        assert_eq!(
            feedback::delete_as_viewer(&conn, &wrong, &viewer, false),
            Err(AppError::NotFound(NOT_FOUND_MESSAGE.to_owned()))
        );
    }

    // In scope, but another viewer's row: 403 — and an administrator overrides it.
    let in_scope = scope(&artifact, &org_id, &mine.id);
    assert_eq!(
        feedback::resolve_as_viewer(&conn, &in_scope, &stranger, false),
        Err(AppError::Forbidden(FORBIDDEN_MESSAGE.to_owned()))
    );
    let mutation = feedback::resolve_as_viewer(&conn, &in_scope, &stranger, true).expect("admin");
    assert!(mutation.changed);
    assert_eq!(
        feedback::get(&conn, &mine.id)
            .expect("read")
            .expect("present")
            .resolved_by
            .as_deref(),
        Some("admin:stranger@example.com"),
        "an administrator's resolution is recorded with the admin: prefix"
    );
    // A retried resolve is not a transition, so the route emits no second notification.
    let retried = feedback::resolve_as_viewer(&conn, &in_scope, &viewer, false).expect("retry");
    assert!(!retried.changed);

    assert!(
        feedback::delete_as_viewer(&conn, &in_scope, &viewer, false)
            .expect("own row")
            .changed
    );
}

#[test]
fn feedback_ref_exposes_only_the_pre_authorization_fields() {
    let fixture = Fixture::new("u11-ref");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("refonly00001", ORG, CLIENT);
    let org_id = org(ORG);
    let body = submission("secret body");
    let item = feedback::add(
        &conn,
        &fixture.ids,
        &new_feedback_in(&artifact, &org_id, &body),
    )
    .expect("add");

    assert_eq!(
        feedback::feedback_ref(&conn, &item.id),
        Ok(Some(FeedbackRef {
            id: item.id.clone(),
            artifact_id: artifact,
            org: org_id,
        }))
    );
    assert_eq!(
        feedback::feedback_ref(&conn, &FeedbackId::from("nothing")),
        Ok(None)
    );
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// Rewrites `created_at`/`resolved_at` so the ordering tests do not depend on wall-clock
/// resolution — SQLite stores whole seconds, so a burst of inserts shares one timestamp.
fn set_times(fixture: &Fixture, id: &FeedbackId, created_at: &str, resolved: bool) {
    fixture
        .conn()
        .execute(
            "UPDATE feedback SET created_at = ?2, resolved_at = ?3, resolved_by = ?4 WHERE id = ?1",
            params![
                id.0,
                created_at,
                resolved.then(|| "2026-02-01 00:00:00".to_owned()),
                resolved.then(|| "agent:key-acme".to_owned())
            ],
        )
        .expect("rewrite timestamps");
}

#[test]
fn listings_put_open_items_first_in_their_frozen_order() {
    let fixture = Fixture::new("u11-order");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("ordered00001", ORG, CLIENT);
    let other = fixture.seed_artifact("ordered00002", ORG, CLIENT);
    let org_id = org(ORG);

    let add = |artifact: &ArtifactId, body: &str| {
        let submission = submission(body);
        feedback::add(
            &conn,
            &fixture.ids,
            &new_feedback_in(artifact, &org_id, &submission),
        )
        .expect("add feedback")
    };

    let old_open = add(&artifact, "old open");
    let new_open = add(&artifact, "new open");
    let old_done = add(&artifact, "old resolved");
    let new_done = add(&artifact, "new resolved");
    let elsewhere = add(&other, "another artifact");
    set_times(&fixture, &old_open.id, "2026-01-01 00:00:00", false);
    set_times(&fixture, &new_open.id, "2026-03-01 00:00:00", false);
    set_times(&fixture, &old_done.id, "2026-01-02 00:00:00", true);
    set_times(&fixture, &new_done.id, "2026-03-02 00:00:00", true);
    set_times(&fixture, &elsewhere.id, "2026-02-01 00:00:00", false);

    // The viewer thread: open first, then oldest-first — reading order.
    assert_eq!(
        bodies(feedback::list_for_artifact(&conn, &artifact).expect("list")),
        ["old open", "new open", "old resolved", "new resolved"]
    );
    // The admin firehose: open first, then newest-first.
    assert_eq!(
        bodies(feedback::list_all(&conn, None).expect("list all")),
        [
            "new open",
            "another artifact",
            "old open",
            "new resolved",
            "old resolved"
        ]
    );
    // `listAll(artifactId)` is the thread order, not the firehose order [lib/feedback.js:154].
    assert_eq!(
        bodies(feedback::list_all(&conn, Some(&artifact)).expect("list all for artifact")),
        ["old open", "new open", "old resolved", "new resolved"]
    );
    // The agent view: newest-first, restricted to the key's own artifacts.
    assert_eq!(
        bodies(
            feedback::list_for_client(&conn, &client(CLIENT), None, Some(&org_id)).expect("client")
        ),
        [
            "new open",
            "another artifact",
            "old open",
            "new resolved",
            "old resolved"
        ]
    );
    assert_eq!(
        bodies(
            feedback::list_for_client(&conn, &client(CLIENT), Some(&artifact), Some(&org_id))
                .expect("client+artifact")
        ),
        ["new open", "old open", "new resolved", "old resolved"]
    );
    // A different key sees none of it.
    assert_eq!(
        feedback::list_for_client(&conn, &client("key-other"), None, Some(&org_id))
            .expect("other key"),
        vec![]
    );
}

#[test]
fn same_second_items_break_ties_on_id_in_each_direction() {
    let fixture = Fixture::new("u11-tiebreak");
    let conn = fixture.conn();
    let artifact = fixture.seed_artifact("tiebreak0001", ORG, CLIENT);
    let org_id = org(ORG);

    // Ascending ids sharing one timestamp: `id ASC` and `id DESC` must disagree.
    let mut ids = Vec::new();
    for index in 0..4 {
        let submission = submission(&format!("item {index}"));
        let item = feedback::add(
            &conn,
            &fixture.ids,
            &new_feedback_in(&artifact, &org_id, &submission),
        )
        .expect("add");
        set_times(&fixture, &item.id, "2026-01-01 00:00:00", false);
        ids.push(item.id.0);
    }
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");

    assert_eq!(
        bodies(feedback::list_for_artifact(&conn, &artifact).expect("list")),
        ["item 0", "item 1", "item 2", "item 3"]
    );
    assert_eq!(
        bodies(feedback::list_all(&conn, None).expect("list all")),
        ["item 3", "item 2", "item 1", "item 0"]
    );
}

#[test]
fn a_non_admin_key_never_sees_the_new_tenants_feedback_after_a_move() {
    let fixture = Fixture::new("u11-move-leak");
    let mut conn = fixture.conn();
    let artifact = fixture.seed_artifact("moveleak0001", ORG, CLIENT);
    let org_id = org(ORG);
    let body = submission("before the move");
    let before = feedback::add(
        &conn,
        &fixture.ids,
        &new_feedback_in(&artifact, &org_id, &body),
    )
    .expect("add");
    assert_eq!(before.org, org_id);

    // `moveArtifactToOrg` moves the artifact and its feedback but keeps `client_id`
    // [lib/store.js:469-478], so the org filter is the only thing between the old key and the new
    // tenant's bodies, anchors, and verified viewer emails.
    let transaction = conn.transaction().expect("begin");
    transaction
        .execute_batch("PRAGMA defer_foreign_keys = ON")
        .expect("defer");
    transaction
        .execute(
            "UPDATE artifacts SET org = 'globex' WHERE id = ?1",
            params![artifact.0],
        )
        .expect("move artifact");
    transaction
        .execute(
            "UPDATE feedback SET org = 'globex' WHERE artifact_id = ?1",
            params![artifact.0],
        )
        .expect("move feedback");
    transaction.commit().expect("commit");

    assert_eq!(
        feedback::list_for_client(&conn, &client(CLIENT), None, Some(&org_id)).expect("scoped"),
        vec![],
        "the original org-scoped key must see nothing"
    );
    // The admin path (`org = None`) still sees everything, which is the intended asymmetry.
    assert_eq!(
        bodies(feedback::list_for_client(&conn, &client(CLIENT), None, None).expect("admin")),
        ["before the move"]
    );
}
