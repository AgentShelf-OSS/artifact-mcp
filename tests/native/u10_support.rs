//! U10 test support: engagement fixtures and the `lib/` Node oracle driver.
//!
//! Every parity assertion in `u10_node_parity.rs` drives the *real* reference modules
//! (`lib/reactions.js`, `lib/views.js`, `lib/notifications.js`, `lib/contracts.js`) through
//! `node -e`, in the same style as `tests/native/u04_crypto.rs`. A Rust-only round trip could not
//! prove that the ported SQL keeps Node's row ordering, aggregate shapes, or error strings.
//!
//! When Node or `node_modules/better-sqlite3` is missing the parity tests **skip** so `cargo test`
//! still works in a Rust-only environment. `REQUIRE_NODE_REFERENCE=1` converts that skip into a
//! hard failure, as required for every cross-runtime proof in CI.

use std::path::{Path, PathBuf};
use std::process::Command;

use artifact_mcp::persistence::db::{self, Database, DbPool};
use artifact_mcp::persistence::migrations::MigrationContext;
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::u03_support::TempDataDir;

/// Repository root (the worktree), used to locate `lib/` and `node_modules/`.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A migrated, pooled database in a throwaway data directory.
pub struct Fixture {
    dir: TempDataDir,
    pool: DbPool,
}

impl Fixture {
    /// Bootstraps an empty database at the latest schema.
    pub fn new(label: &str) -> Self {
        let dir = TempDataDir::new(label);
        let pool = Database::open_with(dir.path(), &MigrationContext::empty(), None)
            .expect("open rust database");
        Self { dir, pool }
    }

    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A pooled connection (`foreign_keys = ON`, so the cascade assertions are real).
    pub fn conn(&self) -> db::DbConnection {
        db::checkout(&self.pool).expect("checkout connection")
    }
}

/// Inserts an artifact row, matching the fixture SQL used by `test/views.test.js:19-21`.
pub fn insert_artifact(conn: &Connection, id: &str, org: &str, title: &str) {
    conn.execute(
        "INSERT INTO artifacts (id, client_id, org, title) VALUES (?, 'publisher', ?, ?)",
        (id, org, title),
    )
    .expect("insert artifact");
}

/// Inserts a feedback row with an explicit timestamp (`test/notifications.test.js:26-32`).
pub fn insert_feedback(
    conn: &Connection,
    id: &str,
    artifact_id: &str,
    org: &str,
    email: &str,
    body: &str,
    created_at: &str,
) {
    conn.execute(
        "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision, created_at)
         VALUES (?, ?, ?, ?, ?, 1, ?)",
        (id, artifact_id, org, email, body, created_at),
    )
    .expect("insert feedback");
}

/// Inserts an `artifact_views` row with fully explicit counters and timestamps, so ordering
/// assertions do not depend on `datetime('now')` resolution.
pub fn insert_view(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
    email: &str,
    count: i64,
    first_viewed_at: &str,
    last_viewed_at: &str,
) {
    conn.execute(
        "INSERT INTO artifact_views (artifact_id, org, email, count, first_viewed_at, last_viewed_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        (artifact_id, org, email, count, first_viewed_at, last_viewed_at),
    )
    .expect("insert artifact view");
}

/// Inserts a reaction row directly (bypassing the adapter, for read-side fixtures).
pub fn insert_reaction(
    conn: &Connection,
    email: &str,
    artifact_id: &str,
    favorite: i64,
    vote: i64,
) {
    conn.execute(
        "INSERT INTO reactions (email, artifact_id, favorite, vote) VALUES (?, ?, ?, ?)",
        (email, artifact_id, favorite, vote),
    )
    .expect("insert reaction");
}

/// Scalar `SELECT` helper.
pub fn scalar<T: rusqlite::types::FromSql>(conn: &Connection, sql: &str) -> T {
    conn.query_row(sql, [], |row| row.get::<_, T>(0))
        .unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
}

/// Every row of a single-column query, in query order.
pub fn column(conn: &Connection, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).expect("prepare query");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("run query");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("read rows")
}

/// Turns an unavailable Node reference into a hard failure instead of a silent skip.
fn require_node_reference() -> bool {
    matches!(std::env::var("REQUIRE_NODE_REFERENCE").as_deref(), Ok("1"))
}

/// Node reference availability. Returns `false` (skip) only when `REQUIRE_NODE_REFERENCE=1` is
/// unset; otherwise the missing oracle fails the test loudly.
pub fn node_reference_available() -> bool {
    let root = repo_root();
    let missing = if !root.join("node_modules/better-sqlite3").is_dir() {
        Some("node_modules/better-sqlite3 is missing")
    } else if !root.join("lib/views.js").is_file() {
        Some("lib/views.js is missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };

    match missing {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "REQUIRE_NODE_REFERENCE=1 but the Node reference is unavailable ({reason}); \
                 the U10 engagement parity proof did not run"
            );
            eprintln!("skipping U10 Node parity proof: {reason}");
            eprintln!("set REQUIRE_NODE_REFERENCE=1 to make this a failure instead");
            false
        }
    }
}

/// One `node -e` program that drives every `lib/` entry point this unit ports.
///
/// `process.argv[1]` is the `lib/` directory URL and `process.argv[2]` the JSON request, matching
/// the convention `u03_cross_runtime.rs` and `u04_crypto.rs` established.
const NODE_DRIVER: &str = r#"
const libBase = process.argv[1];
const input = JSON.parse(process.argv[2]);
// Node reports SQLite booleans as 0/1; the Rust projection types them as bool. The difference is
// representation only (the value is rendered into HTML, never returned as JSON), so it is
// normalised here rather than hidden inside the comparison.
const notification = (row) => ({
  ...row,
  resolved: Boolean(row.resolved),
  has_anchor: Boolean(row.has_anchor),
  unread: Boolean(row.unread)
});
Promise.all([
  import(libBase + "views.js"),
  import(libBase + "reactions.js"),
  import(libBase + "notifications.js"),
  import(libBase + "contracts.js")
]).then(([views, reactions, notifications, contracts]) => {
  const results = input.ops.map((op) => {
    switch (op.kind) {
      case "countsFor":
        return views.countsFor(op.id);
      case "viewersFor":
        return views.viewersFor(op.id);
      case "countsForOrg":
        return [...views.countsForOrg(op.org)].map(([id, c]) => [id, c.views, c.unique_viewers]);
      case "topForOrg":
        return op.limit === undefined ? views.topForOrg(op.org) : views.topForOrg(op.org, op.limit);
      case "record":
        views.record(op.id, op.org, op.email);
        return null;
      case "getReaction":
        return reactions.getReaction(op.email, op.id);
      case "setReaction":
        return reactions.setReaction(op.email, op.id, op.update);
      case "reactionsFor":
        return [...reactions.reactionsFor(op.email)].map(([id, r]) => [id, r.favorite, r.vote]);
      case "sentimentMap":
        return [...reactions.sentimentMap()].map(([id, s]) => [id, s.up, s.down, s.favorites]);
      case "recentForViewer":
        return notifications
          .recentForViewer({ email: op.email, org: op.org, isAdmin: op.isAdmin, limit: op.limit })
          .map(notification);
      case "unreadCount":
        return notifications.unreadCount({ email: op.email, org: op.org, isAdmin: op.isAdmin });
      case "markSeen":
        notifications.markSeen(op.email);
        return null;
      case "parseReaction":
        try {
          return { ok: true, value: contracts.parseReactionInput(op.value) };
        } catch (error) {
          return { ok: false, error: String((error && error.message) || error) };
        }
      default:
        throw new Error("unknown op: " + op.kind);
    }
  });
  console.log(JSON.stringify({ results }));
}).catch((error) => {
  console.error(error);
  process.exit(1);
});
"#;

/// Runs the Node oracle against `data_dir` and returns one JSON value per requested operation.
///
/// `lib/db.js` is a module-level singleton keyed on `DATA_DIR`, so pointing that at the Rust
/// fixture makes the reference operate on the very same SQLite file.
pub fn run_node(data_dir: &Path, ops: Vec<Value>) -> Vec<Value> {
    let root = repo_root();
    let lib_base = format!("file://{}/", root.join("lib").display());
    let request = json!({ "ops": ops }).to_string();

    let output = Command::new("node")
        .current_dir(&root)
        .arg("-e")
        .arg(NODE_DRIVER)
        .arg(&lib_base)
        .arg(&request)
        .env("DATA_DIR", data_dir)
        .env_remove("ORG_EMAIL_DOMAINS")
        .env_remove("WEBHOOK_ENC_KEY")
        .env_remove("ARTIFACT_API_KEYS")
        .output()
        .expect("run node oracle");
    assert!(
        output.status.success(),
        "node oracle failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("node oracle stdout is utf-8");
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("node oracle emitted json");
    parsed
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .expect("node oracle returned results")
}

/// Convenience wrapper for a single-operation oracle call.
pub fn node_op(data_dir: &Path, op: Value) -> Value {
    run_node(data_dir, vec![op])
        .into_iter()
        .next()
        .expect("one result")
}

/// Serializes any model value for comparison against the oracle's JSON.
pub fn as_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("serialize model value")
}
