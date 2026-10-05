use std::collections::BTreeMap;

use artifact_mcp::{
    error::AppError,
    model::{EmailAddress, OrgId, Viewer},
    persistence::{
        collections::{CollectionActor, CollectionPrincipal, CollectionStore, GalleryPreferences},
        db::{self, Database},
    },
    security::audit::MutationAudit,
};

struct Fixture {
    _dir: std::path::PathBuf,
    pool: db::DbPool,
    store: CollectionStore,
}

fn fixture(label: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "artifact-collections-{label}-{}",
        nanoid::nanoid!(12)
    ));
    std::fs::create_dir_all(&dir).expect("temp directory");
    let pool = Database::open_at(&dir).expect("database");
    Fixture {
        _dir: dir,
        pool: pool.clone(),
        store: CollectionStore::new(pool),
    }
}

async fn seed(pool: &db::DbPool, artifacts: &[(&str, &str, &str, i64)]) {
    let rows = artifacts
        .iter()
        .map(|(id, org, owner, hidden)| {
            (
                (*id).to_owned(),
                (*org).to_owned(),
                (*owner).to_owned(),
                *hidden,
            )
        })
        .collect::<Vec<_>>();
    db::interact(pool, move |conn| {
        for org in ["acme", "beta"] {
            conn.execute("INSERT OR IGNORE INTO orgs(name) VALUES (?1)", [org]).map_err(|_| artifact_mcp::error::AppError::Internal)?;
        }
        for (id, org, owner, hidden) in rows {
            conn.execute("INSERT OR REPLACE INTO artifacts(id,client_id,org,title,owner_email,hidden) VALUES (?1,'client',?2,?1,?3,?4)", rusqlite::params![id, org, owner, hidden]).map_err(|_| artifact_mcp::error::AppError::Internal)?;
        }
        Ok(())
    }).await.expect("seed");
}

fn actor(email: &str, org: &str) -> CollectionActor {
    CollectionActor {
        email: email.to_owned(),
        org: org.to_owned(),
        is_admin: false,
        principal: CollectionPrincipal::HumanEmail(email.to_owned()),
        publisher: None,
    }
}

fn audit(viewer: &Viewer) -> MutationAudit {
    MutationAudit::viewer(viewer).expect("audit context")
}

#[tokio::test]
async fn create_and_memberships_are_tenant_scoped_and_bulk_atomic() {
    let fixture = fixture("atomic");
    seed(
        &fixture.pool,
        &[
            ("one", "acme", "alice@acme.test", 0),
            ("hidden", "acme", "bob@acme.test", 1),
            ("foreign", "beta", "other@beta.test", 0),
        ],
    )
    .await;
    let alice = actor("alice@acme.test", "acme");
    let collection = fixture
        .store
        .create(
            &alice,
            "collection-one".into(),
            "Design".into(),
            "".into(),
            None,
            None,
        )
        .await
        .expect("create");
    let error = fixture
        .store
        .add_memberships(
            &alice,
            collection.id.clone(),
            vec!["one".into(), "foreign".into()],
        )
        .await
        .expect_err("foreign target must fail");
    assert!(matches!(
        error,
        AppError::ConcealedNotFound | AppError::NotFound(_)
    ));
    assert!(
        fixture
            .store
            .add_memberships(&alice, collection.id.clone(), vec!["hidden".into()])
            .await
            .is_err(),
        "hidden artifact owned by another viewer must stay concealed"
    );
    assert!(
        fixture
            .store
            .members(&alice, collection.id)
            .await
            .expect("members")
            .is_empty(),
        "mixed target failure must roll back every insert"
    );
}

#[tokio::test]
async fn only_creator_can_change_shared_collection_and_cover_cleanup_is_safe() {
    let fixture = fixture("permissions");
    seed(
        &fixture.pool,
        &[
            ("one", "acme", "alice@acme.test", 0),
            ("two", "acme", "bob@acme.test", 0),
        ],
    )
    .await;
    let alice = actor("alice@acme.test", "acme");
    let bob = actor("bob@acme.test", "acme");
    let collection = fixture
        .store
        .create_atomic(
            &alice,
            "collection-two".into(),
            "Research".into(),
            "".into(),
            None,
            Some("one".into()),
            vec!["one".into()],
            None,
        )
        .await
        .expect("create");
    assert!(matches!(
        fixture
            .store
            .update(&bob, collection.id.clone(), Default::default())
            .await,
        Err(AppError::Forbidden(_))
    ));
    assert!(matches!(
        fixture
            .store
            .add_memberships(&bob, collection.id.clone(), vec!["two".into()])
            .await,
        Err(AppError::Forbidden(_))
    ));
    fixture
        .store
        .remove_memberships(&alice, collection.id.clone(), vec!["one".into()])
        .await
        .expect("remove");
    let row = fixture
        .store
        .list(&alice, 10)
        .await
        .expect("list")
        .pop()
        .expect("row");
    assert_eq!(row.cover_artifact_id, None);
}

#[tokio::test]
async fn membership_limit_and_artifact_lifecycle_cleanup_are_enforced() {
    let fixture = fixture("limits");
    let mut rows = Vec::new();
    for index in 0..1001 {
        let id = format!("a{index}");
        rows.push((
            Box::leak(id.into_boxed_str()) as &str,
            "acme",
            "alice@acme.test",
            0,
        ));
    }
    seed(&fixture.pool, &rows).await;
    let alice = actor("alice@acme.test", "acme");
    let collection = fixture
        .store
        .create(
            &alice,
            "collection-three".into(),
            "Large".into(),
            "".into(),
            None,
            None,
        )
        .await
        .expect("create");
    let too_many = (0..101)
        .map(|index| format!("a{index}"))
        .collect::<Vec<_>>();
    assert!(matches!(
        fixture
            .store
            .add_memberships(&alice, collection.id.clone(), too_many)
            .await,
        Err(AppError::PayloadTooLarge | AppError::Validation(_))
    ));
    for batch in (0..1000).collect::<Vec<_>>().chunks(100) {
        fixture
            .store
            .add_memberships(
                &alice,
                collection.id.clone(),
                batch.iter().map(|index| format!("a{index}")).collect(),
            )
            .await
            .expect("membership batch");
    }
    assert!(
        fixture
            .store
            .add_memberships(&alice, collection.id.clone(), vec!["a1000".into()])
            .await
            .is_err(),
        "collection membership cap"
    );
    db::interact(&fixture.pool, move |conn| {
        conn.execute("UPDATE artifacts SET org='beta' WHERE id='a0'", [])
            .map_err(|_| AppError::Internal)?;
        Ok(())
    })
    .await
    .expect("move artifact");
    assert!(
        !fixture
            .store
            .members(&alice, collection.id.clone())
            .await
            .expect("members")
            .contains(&"a0".to_owned())
    );
}

#[tokio::test]
async fn preferences_are_pruned_to_authorized_collections_and_audited_mutations_roll_back() {
    let fixture = fixture("preferences");
    seed(&fixture.pool, &[("one", "acme", "alice@acme.test", 0)]).await;
    let alice = actor("alice@acme.test", "acme");
    let collection = fixture
        .store
        .create(
            &alice,
            "collection-four".into(),
            "Saved".into(),
            "".into(),
            None,
            None,
        )
        .await
        .expect("create");
    let mut preferences = GalleryPreferences {
        view: "ribbons".into(),
        ..Default::default()
    };
    preferences.collection_order_by_org = BTreeMap::from([
        ("acme".into(), vec![collection.id.clone(), "stale".into()]),
        ("beta".into(), vec!["foreign".into()]),
    ]);
    let saved = fixture
        .store
        .save_preferences(&alice, preferences)
        .await
        .expect("preferences");
    assert_eq!(
        saved.collection_order_by_org.get("acme").unwrap(),
        &vec![collection.id.clone()]
    );
    assert!(!saved.collection_order_by_org.contains_key("beta"));

    let audited = CollectionStore::with_audit(fixture.pool.clone(), [7; 32]);
    let viewer = Viewer {
        email: Some(EmailAddress::from("alice@acme.test")),
        org: Some(OrgId::from("acme")),
        is_admin: false,
    };
    let audit = audit(&viewer);
    db::interact(&fixture.pool, |conn| { conn.execute("CREATE TRIGGER fail_collection_audit AFTER INSERT ON security_audit_events BEGIN SELECT RAISE(ABORT, 'audit unavailable'); END", []).map_err(|_| AppError::Internal)?; Ok(()) }).await.expect("trigger");
    assert!(
        audited
            .create_atomic(
                &alice,
                "collection-five".into(),
                "Rollback".into(),
                "".into(),
                None,
                None,
                Vec::new(),
                Some(audit)
            )
            .await
            .is_err()
    );
    db::interact(&fixture.pool, |conn| {
        conn.execute("DROP TRIGGER fail_collection_audit", [])
            .map_err(|_| AppError::Internal)?;
        Ok(())
    })
    .await
    .expect("drop trigger");
    assert!(
        fixture
            .store
            .list(&alice, 20)
            .await
            .expect("list")
            .iter()
            .all(|row| row.name != "Rollback")
    );
}
