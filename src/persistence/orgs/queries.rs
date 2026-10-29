use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension as _};

use super::semantics::database_failure;
use super::semantics::{norm_domain, norm_email, norm_org};
use crate::error::AppError;
use crate::model::{OrgId, Organization, Timestamp};

// ---------------------------------------------------------------------------
// Synchronous operations — one connection, no `.await` (see `db::interact`)
// ---------------------------------------------------------------------------

/// `orgExists(name)` — [lib/orgs.js:53-55]
///
/// # Errors
/// [`AppError::Internal`] when the query fails.
pub fn org_exists(conn: &Connection, name: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM orgs WHERE name = ?",
        [norm_org(name)],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(|error| database_failure("org exists", &error))
}

/// `orgForDomain(domain)` — [lib/orgs.js:58-61]
///
/// # Errors
/// [`AppError::Internal`] when the query fails.
pub fn org_for_domain(conn: &Connection, domain: &str) -> Result<Option<OrgId>, AppError> {
    domain_owner(conn, &norm_domain(domain)).map(|org| org.map(OrgId))
}

/// `orgForEmail(email)` — [lib/orgs.js:64-67]
///
/// The v21 column is `COLLATE NOCASE`, so an un-normalized address would still match; the
/// normalization is kept anyway because Node performs it before the query.
///
/// # Errors
/// [`AppError::Internal`] when the query fails.
pub fn org_for_email(conn: &Connection, email: &str) -> Result<Option<OrgId>, AppError> {
    email_owner(conn, &norm_email(email)).map(|org| org.map(OrgId))
}

/// `categoriesFor(org)` — [lib/orgs.js:69-71]
///
/// # Errors
/// [`AppError::Internal`] when the query fails.
pub fn categories(conn: &Connection, org: &str) -> Result<Vec<String>, AppError> {
    string_column(
        conn,
        "SELECT name FROM org_categories WHERE org = ? ORDER BY name ASC",
        &norm_org(org),
    )
    .map_err(|error| database_failure("list categories", &error))
}

/// `listOrgNames()` — [lib/orgs.js:73-75]
///
/// # Errors
/// [`AppError::Internal`] when the query fails.
pub fn org_names(conn: &Connection) -> Result<Vec<OrgId>, AppError> {
    let mut statement = conn
        .prepare("SELECT name FROM orgs ORDER BY name ASC")
        .map_err(|error| database_failure("list org names", &error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0).map(OrgId))
        .map_err(|error| database_failure("list org names", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| database_failure("list org names", &error))
}

/// `listOrgs()` — [lib/orgs.js:77-89]
///
/// # Errors
/// [`AppError::Internal`] when any of the underlying queries fails.
pub fn list_orgs(conn: &Connection) -> Result<Vec<Organization>, AppError> {
    let key_counts = active_key_counts(conn)?;
    let mut statement = conn
        .prepare("SELECT name, label, color, created_at FROM orgs ORDER BY name ASC")
        .map_err(|error| database_failure("list orgs", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|error| database_failure("list orgs", &error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| database_failure("list orgs", &error))?;

    let mut organizations = Vec::with_capacity(rows.len());
    for (name, label, color, created_at) in rows {
        organizations.push(Organization {
            domains: domains_for(conn, &name)?,
            emails: emails_for(conn, &name)?,
            categories: categories(conn, &name)?,
            key_count: key_counts.get(&name).copied().unwrap_or(0),
            // `o.color || null`: `setColor` stores NULL for "cleared", but a legacy row could
            // hold `''`, which Node also reports as `null`.
            color: color.filter(|value| !value.is_empty()),
            created_at: created_at.map(Timestamp),
            name: OrgId(name),
            label,
        });
    }
    Ok(organizations)
}

/// `colorMap()` — [lib/orgs.js:92-96]
///
/// # Errors
/// [`AppError::Internal`] when the query fails.
pub fn color_map(conn: &Connection) -> Result<BTreeMap<OrgId, Option<String>>, AppError> {
    let mut statement = conn
        .prepare("SELECT name, color FROM orgs ORDER BY name ASC")
        .map_err(|error| database_failure("color map", &error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                OrgId(row.get::<_, String>(0)?),
                row.get::<_, Option<String>>(1)?
                    .filter(|value| !value.is_empty()),
            ))
        })
        .map_err(|error| database_failure("color map", &error))?;
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .map_err(|error| database_failure("color map", &error))
}

// ---------------------------------------------------------------------------
// Small query helpers
// ---------------------------------------------------------------------------

fn string_column(conn: &Connection, sql: &str, parameter: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map([parameter], |row| row.get::<_, String>(0))?;
    rows.collect()
}

fn domains_for(conn: &Connection, org: &str) -> Result<Vec<String>, AppError> {
    string_column(
        conn,
        "SELECT domain FROM org_domains WHERE org = ? ORDER BY domain ASC",
        org,
    )
    .map_err(|error| database_failure("list org domains", &error))
}

fn emails_for(conn: &Connection, org: &str) -> Result<Vec<String>, AppError> {
    string_column(
        conn,
        "SELECT email FROM org_email_members WHERE org = ? ORDER BY email ASC",
        org,
    )
    .map_err(|error| database_failure("list org emails", &error))
}

/// `activeKeyCountsStmt` — [lib/orgs.js:14-16]
fn active_key_counts(conn: &Connection) -> Result<BTreeMap<String, u64>, AppError> {
    let mut statement = conn
        .prepare("SELECT org, COUNT(*) AS n FROM api_keys WHERE revoked_at IS NULL GROUP BY org")
        .map_err(|error| database_failure("count active keys", &error))?;
    let rows = statement
        .query_map([], |row| {
            // `COUNT(*)` is a non-negative SQLite integer; the clamp only guards a corrupt read.
            let count = u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0);
            Ok((row.get::<_, String>(0)?, count))
        })
        .map_err(|error| database_failure("count active keys", &error))?;
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .map_err(|error| database_failure("count active keys", &error))
}

pub(super) fn domain_owner(conn: &Connection, domain: &str) -> Result<Option<String>, AppError> {
    conn.query_row(
        "SELECT org FROM org_domains WHERE domain = ?",
        [domain],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| database_failure("read domain owner", &error))
}

pub(super) fn email_owner(conn: &Connection, email: &str) -> Result<Option<String>, AppError> {
    conn.query_row(
        "SELECT org FROM org_email_members WHERE email = ?",
        [email],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| database_failure("read email owner", &error))
}
