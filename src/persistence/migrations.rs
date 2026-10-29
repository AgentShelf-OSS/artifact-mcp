//! Owned by U03 (terra) — append-only version/name-compatible SQLite migrations.
//!
//! The Node reference `lib/migrations.js` is the behavioural oracle for this module. Every
//! statement below is a verbatim copy of the SQL it executes so that `sqlite_master.sql` text,
//! column order, index names, check constraints, and composite foreign keys are identical
//! between a Node-created and a Rust-created database. Do not "tidy" the SQL: the schema
//! comparison in `tests/native/u03_migrations.rs` checks normalised statement text against a
//! database the Node runtime wrote, and a reformatted statement is a real divergence in a shared
//! production database.
//!
//! Blocking discipline: every function here is synchronous `rusqlite` work. Callers must run
//! them inside `tokio::task::spawn_blocking` (see [`crate::persistence::db::interact`]); no
//! function in this module may be awaited or called while a transaction is held across `.await`.

mod m001_m010;
mod m011_m024;
mod m025_m027;
mod m028_m029;
mod m030_m031;
mod m032_m040;
mod schema_helpers;
mod webhook_urls;

use rusqlite::{Connection, Transaction};

use m001_m010::{
    m001_initial_schema, m002_org_label_and_bundles, m003_reaction_integrity,
    m004_artifact_revision, m005_viewer_feedback, m006_artifact_category, m007_org_registry,
    m008_artifact_history, m009_org_discord_webhooks, m010_artifact_view_analytics,
};
use m011_m024::{
    m011_artifact_visibility, m012_feedback_anchors, m013_feedback_threads,
    m014_feedback_anchor_boxes, m015_artifact_public_shares, m016_org_color,
    m017_artifact_body_digest, m018_webhook_url_encryption, m019_feedback_anchor_page,
    m020_notification_read_watermarks, m021_explicit_email_org_membership,
    m022_api_key_capabilities, m023_verified_artifact_owner, m024_artifact_durability_intents,
};
use m025_m027::{
    m025_security_audit_ledger, m026_security_audit_protocol_hardening,
    m027_provider_delivery_outbox,
};
use m028_m029::{m028_discord_discussion_mirror, m029_discord_notification_threads};
use m030_m031::{m030_discord_organization_threading_policy, m031_discord_two_way_inbound_sync};
use m032_m040::{
    m32_feedback_anchor_v2, m033_artifact_state, m034_viewer_state_scope,
    m035_artifact_data_bindings, m036_managed_data_sources,
    m037_artifact_collections_and_viewer_preferences, m038_collection_principals,
    m039_web_push_reminders, m040_artifact_keys,
};
use schema_helpers::table_exists;
pub use webhook_urls::{
    EncryptedUrl, WebhookUrlCipher, encrypt_plaintext_webhook_urls, mask_webhook_url,
};

/// Latest schema version. The ledger is append-only and must match Node exactly.
pub const LATEST_SCHEMA_VERSION: i64 = 40;

/// Environment-derived inputs consumed by migrations with programmatic side effects.
///
/// Migration 7 (`org-registry`) seeds `orgs`/`org_domains` from `ORG_EMAIL_DOMAINS`, exactly as
/// `lib/migrations.js` does. Capturing that value in an explicit context keeps migrations
/// deterministic and testable instead of reading process state deep inside the ladder.
#[derive(Clone, Debug, Default)]
pub struct MigrationContext {
    /// Raw `ORG_EMAIL_DOMAINS` value in `domain:org,domain:org` form.
    pub org_email_domains: String,
}

impl MigrationContext {
    /// Mirrors `String(process.env.ORG_EMAIL_DOMAINS || "")` in the Node reference.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            org_email_domains: std::env::var("ORG_EMAIL_DOMAINS").unwrap_or_default(),
        }
    }

    /// Context with no environment-derived seeding, used by fixtures and tests.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

type MigrationFn = fn(&Transaction<'_>, &MigrationContext) -> rusqlite::Result<()>;

/// One ordered, append-only schema step recorded in `schema_migrations`.
pub struct Migration {
    /// Frozen version number.
    pub version: i64,
    /// Frozen migration name.
    pub name: &'static str,
    up: MigrationFn,
}

impl Migration {
    /// Applies this migration's statements inside the caller's transaction.
    fn run(&self, tx: &Transaction<'_>, ctx: &MigrationContext) -> rusqlite::Result<()> {
        (self.up)(tx, ctx)
    }
}

/// A migration applied by this process (not one already recorded in the database).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    /// Applied version.
    pub version: i64,
    /// Applied name.
    pub name: &'static str,
}

/// The frozen ledger. Append only; never renumber, rename, reorder, or edit an existing entry.
pub static MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial-schema",
        up: m001_initial_schema,
    },
    Migration {
        version: 2,
        name: "org-label-and-bundles",
        up: m002_org_label_and_bundles,
    },
    Migration {
        version: 3,
        name: "reaction-integrity",
        up: m003_reaction_integrity,
    },
    Migration {
        version: 4,
        name: "artifact-revision",
        up: m004_artifact_revision,
    },
    Migration {
        version: 5,
        name: "viewer-feedback",
        up: m005_viewer_feedback,
    },
    Migration {
        version: 6,
        name: "artifact-category",
        up: m006_artifact_category,
    },
    Migration {
        version: 7,
        name: "org-registry",
        up: m007_org_registry,
    },
    Migration {
        version: 8,
        name: "artifact-history",
        up: m008_artifact_history,
    },
    Migration {
        version: 9,
        name: "org-discord-webhooks",
        up: m009_org_discord_webhooks,
    },
    Migration {
        version: 10,
        name: "artifact-view-analytics",
        up: m010_artifact_view_analytics,
    },
    Migration {
        version: 11,
        name: "artifact-visibility",
        up: m011_artifact_visibility,
    },
    Migration {
        version: 12,
        name: "feedback-anchors",
        up: m012_feedback_anchors,
    },
    Migration {
        version: 13,
        name: "feedback-threads",
        up: m013_feedback_threads,
    },
    Migration {
        version: 14,
        name: "feedback-anchor-boxes",
        up: m014_feedback_anchor_boxes,
    },
    Migration {
        version: 15,
        name: "artifact-public-shares",
        up: m015_artifact_public_shares,
    },
    Migration {
        version: 16,
        name: "org-color",
        up: m016_org_color,
    },
    Migration {
        version: 17,
        name: "artifact-body-digest",
        up: m017_artifact_body_digest,
    },
    Migration {
        version: 18,
        name: "webhook-url-encryption",
        up: m018_webhook_url_encryption,
    },
    Migration {
        version: 19,
        name: "feedback-anchor-page",
        up: m019_feedback_anchor_page,
    },
    Migration {
        version: 20,
        name: "notification-read-watermarks",
        up: m020_notification_read_watermarks,
    },
    Migration {
        version: 21,
        name: "explicit-email-org-membership",
        up: m021_explicit_email_org_membership,
    },
    Migration {
        version: 22,
        name: "api-key-capabilities",
        up: m022_api_key_capabilities,
    },
    Migration {
        version: 23,
        name: "verified-artifact-owner",
        up: m023_verified_artifact_owner,
    },
    Migration {
        version: 24,
        name: "artifact-durability-intents",
        up: m024_artifact_durability_intents,
    },
    Migration {
        version: 25,
        name: "security-audit-ledger",
        up: m025_security_audit_ledger,
    },
    Migration {
        version: 26,
        name: "security-audit-protocol-hardening",
        up: m026_security_audit_protocol_hardening,
    },
    Migration {
        version: 27,
        name: "provider-delivery-outbox",
        up: m027_provider_delivery_outbox,
    },
    Migration {
        version: 28,
        name: "discord-discussion-mirror",
        up: m028_discord_discussion_mirror,
    },
    Migration {
        version: 29,
        name: "discord-notification-threads",
        up: m029_discord_notification_threads,
    },
    Migration {
        version: 30,
        name: "discord-organization-threading-policy",
        up: m030_discord_organization_threading_policy,
    },
    Migration {
        version: 31,
        name: "discord-two-way-inbound-sync",
        up: m031_discord_two_way_inbound_sync,
    },
    Migration {
        version: 32,
        name: "feedback-anchor-v2",
        up: m32_feedback_anchor_v2,
    },
    Migration {
        version: 33,
        name: "viewer-state",
        up: m033_artifact_state,
    },
    Migration {
        version: 34,
        name: "viewer-state-scope-and-member-display-name",
        up: m034_viewer_state_scope,
    },
    Migration {
        version: 35,
        name: "artifact-data-bindings",
        up: m035_artifact_data_bindings,
    },
    Migration {
        version: 36,
        name: "managed-data-sources",
        up: m036_managed_data_sources,
    },
    Migration {
        version: 37,
        name: "artifact-collections-and-gallery-preferences",
        up: m037_artifact_collections_and_viewer_preferences,
    },
    Migration {
        version: 38,
        name: "collection-principals",
        up: m038_collection_principals,
    },
    Migration {
        version: 39,
        name: "web-push-reminders",
        up: m039_web_push_reminders,
    },
    Migration {
        version: 40,
        name: "artifact-keys",
        up: m040_artifact_keys,
    },
];

/// Creates `schema_migrations` if needed and applies every unrecorded migration in order.
///
/// Each migration runs in its own transaction together with its `schema_migrations` row, exactly
/// like `db.transaction(...)` in `lib/migrations.js`: a failing migration leaves neither partial
/// DDL nor a bookkeeping row behind. Reopening an up-to-date database applies nothing and returns
/// an empty vector.
///
/// This is blocking work; call it from `spawn_blocking` or from a synchronous bootstrap path.
pub fn apply(
    conn: &mut Connection,
    ctx: &MigrationContext,
) -> rusqlite::Result<Vec<AppliedMigration>> {
    migrate_through(conn, ctx, LATEST_SCHEMA_VERSION)
}

/// Applies the ledger exactly like [`apply`], but stops after `target_version`.
///
/// Production startup always calls [`apply`], which is this function with
/// [`LATEST_SCHEMA_VERSION`]. The bound exists so release tooling (`cargo xtask
/// historical-fixture`) can build a database as it stood at an earlier schema boundary. It runs
/// the real migration functions and records the real ledger rows; it never makes an old database
/// by deleting rows from a newer one. A `target_version` of `0` creates only `schema_migrations`.
///
/// # Errors
///
/// Returns an `SQLITE_RANGE` failure, before it touches the database, when `target_version` is
/// negative or newer than [`LATEST_SCHEMA_VERSION`]. Other errors come from SQLite.
pub fn migrate_through(
    conn: &mut Connection,
    ctx: &MigrationContext,
    target_version: i64,
) -> rusqlite::Result<Vec<AppliedMigration>> {
    if !(0..=LATEST_SCHEMA_VERSION).contains(&target_version) {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_RANGE),
            Some(format!(
                "unsupported schema version {target_version}; expected 0..={LATEST_SCHEMA_VERSION}"
            )),
        ));
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
      version    INTEGER PRIMARY KEY,
      name       TEXT NOT NULL,
      applied_at TEXT NOT NULL DEFAULT (datetime('now'))
    )",
    )?;

    let applied = applied_versions(conn)?;
    let mut newly_applied = Vec::new();

    for migration in MIGRATIONS {
        if migration.version > target_version {
            break;
        }
        if applied.contains(&migration.version) {
            continue;
        }
        let tx = conn.transaction()?;
        migration.run(&tx, ctx)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name) VALUES (?1, ?2)",
            (migration.version, migration.name),
        )?;
        tx.commit()?;
        newly_applied.push(AppliedMigration {
            version: migration.version,
            name: migration.name,
        });
    }

    Ok(newly_applied)
}

/// Versions already recorded in `schema_migrations`, ascending.
pub fn applied_versions(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    if !table_exists(conn, "schema_migrations")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    rows.collect()
}

/// Highest recorded schema version, or `0` for an empty or pre-migration database.
pub fn current_version(conn: &Connection) -> rusqlite::Result<i64> {
    Ok(applied_versions(conn)?.last().copied().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_is_ordered_dense_and_matches_the_frozen_version() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version,
                i64::try_from(index).expect("index fits i64") + 1,
                "migration {} is out of order",
                migration.name
            );
            assert!(!migration.name.is_empty());
        }
        assert_eq!(
            MIGRATIONS.len(),
            usize::try_from(LATEST_SCHEMA_VERSION).expect("schema version fits usize")
        );
        assert_eq!(
            MIGRATIONS.last().map(|migration| migration.version),
            Some(LATEST_SCHEMA_VERSION)
        );
    }

    fn schema_text(conn: &Connection) -> Vec<(String, String, Option<String>)> {
        let mut stmt = conn
            .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY rowid")
            .expect("prepare sqlite_master");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query sqlite_master")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read sqlite_master")
    }

    #[test]
    fn migrate_through_stops_at_the_boundary_and_apply_resumes_from_it() {
        let ctx = MigrationContext::empty();
        for boundary in [
            0,
            1,
            7,
            24,
            LATEST_SCHEMA_VERSION - 1,
            LATEST_SCHEMA_VERSION,
        ] {
            let mut conn = Connection::open_in_memory().expect("open in-memory database");
            let applied = migrate_through(&mut conn, &ctx, boundary).expect("migrate through");
            assert_eq!(
                applied.iter().map(|m| m.version).collect::<Vec<_>>(),
                (1..=boundary).collect::<Vec<_>>()
            );
            assert_eq!(current_version(&conn).expect("version"), boundary);
            assert!(table_exists(&conn, "schema_migrations").expect("ledger table"));
            assert!(
                migrate_through(&mut conn, &ctx, boundary)
                    .expect("reapply boundary")
                    .is_empty()
            );

            // The normal startup path continues from the boundary to the same schema that a
            // fresh database reaches.
            let resumed = apply(&mut conn, &ctx).expect("apply after boundary");
            assert_eq!(
                resumed.iter().map(|m| m.version).collect::<Vec<_>>(),
                ((boundary + 1)..=LATEST_SCHEMA_VERSION).collect::<Vec<_>>()
            );
            let mut fresh = Connection::open_in_memory().expect("open fresh database");
            apply(&mut fresh, &ctx).expect("fresh apply");
            assert_eq!(
                schema_text(&conn),
                schema_text(&fresh),
                "boundary {boundary}"
            );
        }
    }

    #[test]
    fn migrate_through_rejects_unknown_versions_without_touching_the_database() {
        let ctx = MigrationContext::empty();
        for version in [-1, LATEST_SCHEMA_VERSION + 1] {
            let mut conn = Connection::open_in_memory().expect("open in-memory database");
            let error = migrate_through(&mut conn, &ctx, version).expect_err("out of range");
            assert_eq!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::ParameterOutOfRange)
            );
            assert!(schema_text(&conn).is_empty(), "version {version}");
        }
    }

    #[test]
    fn masks_webhook_urls_like_the_node_reference() {
        assert_eq!(
            mask_webhook_url("https://discord.com/api/webhooks/123/existing-plaintext-token"),
            "https://discord.com/…oken"
        );
        assert_eq!(
            mask_webhook_url("https://hooks.example.com:8443/abc/secret"),
            "https://hooks.example.com:8443/…cret"
        );
        assert_eq!(mask_webhook_url("not a url"), "… url");
        assert_eq!(mask_webhook_url("abc"), "…abc");
        assert_eq!(mask_webhook_url(""), "…");
    }
}
