//! Owned by U09 (terra) — organization, membership, category, and color persistence.
//!
//! Node oracle: `lib/orgs.js` in full. Every validation rule, normalization step, and error
//! string below is a faithful port; the admin routes turn each thrown `Error` into
//! `400 {"error": message}` ([lib/app.js:308-310], [lib/app.js:329-331], [lib/app.js:348-350],
//! [lib/app.js:369-371], [lib/app.js:387-389]), so a message that differs by one character is a
//! user-visible divergence. Thrown errors therefore map to [`AppError::Validation`], whose HTTP
//! mapping is 400 with the supplied message.
//!
//! # Ordering is part of the contract
//!
//! Node checks preconditions in a fixed order and returns on the first failure, so the *message*
//! a caller sees depends on that order (for example `addCategory` reports the unknown org before
//! it reports an empty category name — [lib/orgs.js:172-173]). The sync operations below keep the
//! same sequence.
//!
//! # Case handling
//!
//! * Org ids are case-folded on create ([lib/orgs.js:111]) but matched *exactly* everywhere else
//!   (`orgExists` only trims — [lib/orgs.js:54]), because migration v7 can seed mixed-case orgs
//!   from `api_keys`/`artifacts`.
//! * Domains and emails are trimmed and lowercased ([lib/orgs.js:33-38]).
//! * The v21 `org_email_members.email` column is `COLLATE NOCASE`, so membership lookups and
//!   deletes are case-insensitive *in SQLite as well* — the normalization and the collation are
//!   two independent layers and both are preserved.
//!
//! # Deliberate divergence (contract-delta request)
//!
//! `String.prototype.slice(0, n)` counts UTF-16 code units and will happily cut a surrogate pair
//! in half; better-sqlite3 then stores the lone surrogate as the invalid UTF-8 sequence
//! `ED A0 BD` (verified empirically). A Rust `String` cannot hold that, so [`truncate_utf16`]
//! drops the straddling character instead. Only reachable with an astral character that starts
//! at exactly the label/category cap.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior};

use crate::error::AppError;
use crate::model::{CreateOrganization, EmailAddress, OrgId, Organization, Timestamp};
use crate::persistence::db::{self, DbPool};
use crate::security::audit::{AuditEvent, MutationAudit, mutate_in_transaction};

// ---------------------------------------------------------------------------
// Shared JavaScript string semantics
// ---------------------------------------------------------------------------

/// The exact set matched by JavaScript's `\s` and stripped by `String.prototype.trim`.
///
/// ECMA-262 defines both as `WhiteSpace ∪ LineTerminator`. This is **not** Rust's
/// `char::is_whitespace` (Unicode `White_Space`): it includes U+FEFF (`<ZWNBSP>`) and excludes
/// U+0085 (`<NEL>`). Getting this wrong changes which inputs normalize to the empty string.
#[must_use]
pub fn is_js_whitespace(value: char) -> bool {
    matches!(
        value,
        '\u{9}'          // <TAB>
            | '\u{b}'    // <VT>
            | '\u{c}'    // <FF>
            | '\u{20}'   // <SP>
            | '\u{a0}'   // <NBSP>
            | '\u{feff}' // <ZWNBSP>
            | '\u{a}'    // <LF>
            | '\u{d}'    // <CR>
            | '\u{2028}' // <LS>
            | '\u{2029}' // <PS>
    ) || matches!(
        value,
        // <USP>: the Unicode `Space_Separator` category, minus U+0020/U+00A0 above.
        '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// `String.prototype.trim()`.
#[must_use]
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// Length of `value` in UTF-16 code units, which is what JavaScript's `.length` reports.
#[must_use]
pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// `value.slice(0, max_units)` measured in UTF-16 code units.
///
/// See the divergence note in the module docs: a character that straddles the cap is dropped
/// rather than truncated to a lone surrogate.
#[must_use]
pub fn truncate_utf16(value: &str, max_units: usize) -> String {
    let mut units = 0;
    let mut end = value.len();
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if units + width > max_units {
            end = index;
            break;
        }
        units += width;
    }
    value[..end].to_owned()
}

// ---------------------------------------------------------------------------
// Validation and normalization — [lib/orgs.js:29-51]
// ---------------------------------------------------------------------------

/// Longest accepted org id: `/^[a-z0-9][a-z0-9._-]{0,40}$/i` — [lib/orgs.js:29]
pub const ORG_NAME_MAX_LENGTH: usize = 41;

/// `label.trim().slice(0, 80)` — [lib/orgs.js:112]
pub const ORG_LABEL_MAX_LENGTH: usize = 80;

/// `normCategory` truncation — [lib/orgs.js:50]
pub const CATEGORY_MAX_LENGTH: usize = 60;

/// `email.length > 254` — [lib/orgs.js:40]
pub const EMAIL_MAX_LENGTH: usize = 254;

/// `local.length <= 64` — [lib/orgs.js:45]
pub const EMAIL_LOCAL_MAX_LENGTH: usize = 64;

/// `domain.length <= 253` — [lib/orgs.js:47]
pub const EMAIL_DOMAIN_MAX_LENGTH: usize = 253;

/// The org id reserved for the administrator pseudo-tenant — [lib/orgs.js:114]
pub const RESERVED_ORG_NAME: &str = "admin";

/// `ORG_RE = /^[a-z0-9][a-z0-9._-]{0,40}$/i` — [lib/orgs.js:29]
#[must_use]
pub fn is_valid_org_name(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.clone().count() < ORG_NAME_MAX_LENGTH
        && characters.all(is_org_name_character)
}

fn is_org_name_character(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-')
}

/// `DOMAIN_RE = /^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/i`
/// — [lib/orgs.js:30]
///
/// The trailing `+` makes at least one dot mandatory, so a bare host name is never a domain.
#[must_use]
pub fn is_valid_domain(value: &str) -> bool {
    let mut labels = value.split('.');
    let first = labels.next().unwrap_or_default();
    let mut count = 0_usize;
    for label in labels {
        if !is_valid_domain_label(label) {
            return false;
        }
        count += 1;
    }
    count >= 1 && is_valid_domain_label(first)
}

/// One `[a-z0-9]([a-z0-9-]*[a-z0-9])?` label: alphanumeric ends, hyphens allowed inside.
fn is_valid_domain_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(first), Some(last)) => {
            first.is_ascii_alphanumeric()
                && last.is_ascii_alphanumeric()
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        }
        _ => false,
    }
}

/// ``EMAIL_LOCAL_RE = /^[a-z0-9!#$%&'*+/=?^_`{|}~.-]+$/i`` — [lib/orgs.js:31]
fn is_valid_email_local(local: &str) -> bool {
    !local.is_empty()
        && local.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '/'
                        | '='
                        | '?'
                        | '^'
                        | '_'
                        | '`'
                        | '{'
                        | '|'
                        | '}'
                        | '~'
                        | '.'
                        | '-'
                )
        })
}

/// `validEmail(email)` — [lib/orgs.js:39-48]. Expects an already-normalized address.
#[must_use]
pub fn is_valid_email(email: &str) -> bool {
    if email.is_empty() || utf16_len(email) > EMAIL_MAX_LENGTH {
        return false;
    }
    if email.chars().any(is_js_whitespace) {
        return false;
    }
    let Some(at) = email.find('@') else {
        return false;
    };
    // `at <= 0 || at !== email.lastIndexOf("@")` — an empty local part or a second `@`.
    if at == 0 || email.rfind('@') != Some(at) {
        return false;
    }
    let (local, domain) = (&email[..at], &email[at + 1..]);
    utf16_len(local) <= EMAIL_LOCAL_MAX_LENGTH
        && is_valid_email_local(local)
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && utf16_len(domain) <= EMAIL_DOMAIN_MAX_LENGTH
        && is_valid_domain(domain)
}

/// `HEX_RE = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/` — [lib/orgs.js:10]
#[must_use]
pub fn is_valid_color(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('#') else {
        return false;
    };
    matches!(digits.len(), 3 | 6) && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `String(name || "").trim()` — the org id normalization used by every lookup
/// ([lib/orgs.js:54] and friends). Deliberately does **not** lowercase.
#[must_use]
pub fn norm_org(value: &str) -> String {
    js_trim(value).to_owned()
}

/// `normDomain` — [lib/orgs.js:33-35]
#[must_use]
pub fn norm_domain(value: &str) -> String {
    js_trim(value).to_lowercase()
}

/// `normEmail` — [lib/orgs.js:36-38]
#[must_use]
pub fn norm_email(value: &str) -> String {
    js_trim(value).to_lowercase()
}

/// `normCategory` — [lib/orgs.js:49-51]: trim, collapse whitespace runs to one space, cap at 60.
#[must_use]
pub fn norm_category(value: &str) -> String {
    let trimmed = js_trim(value);
    let mut collapsed = String::with_capacity(trimmed.len());
    let mut in_whitespace = false;
    for character in trimmed.chars() {
        if is_js_whitespace(character) {
            if !in_whitespace {
                collapsed.push(' ');
                in_whitespace = true;
            }
        } else {
            collapsed.push(character);
            in_whitespace = false;
        }
    }
    truncate_utf16(&collapsed, CATEGORY_MAX_LENGTH)
}

// ---------------------------------------------------------------------------
// Error messages — one helper per Node `throw`
// ---------------------------------------------------------------------------

/// `Unknown organization "<org>".` — [lib/orgs.js:102], [lib/orgs.js:136], [lib/orgs.js:153],
/// [lib/orgs.js:172]
#[must_use]
pub fn unknown_org_message(org: &str) -> String {
    format!("Unknown organization \"{org}\".")
}

fn unknown_org(org: &str) -> AppError {
    AppError::Validation(unknown_org_message(org))
}

/// `"<domain>" is not a valid email domain.` — [lib/orgs.js:118], [lib/orgs.js:137]
fn invalid_domain(domain: &str) -> AppError {
    AppError::Validation(format!("\"{domain}\" is not a valid email domain."))
}

/// [lib/orgs.js:140]
fn domain_taken(domain: &str, owner: &str, same_org: bool) -> AppError {
    AppError::Validation(if same_org {
        format!("\"{domain}\" is already on this org.")
    } else {
        format!("Domain \"{domain}\" is already mapped to \"{owner}\".")
    })
}

/// [lib/orgs.js:157-159]
fn email_taken(email: &str, owner: &str, same_org: bool) -> AppError {
    AppError::Validation(if same_org {
        format!("\"{email}\" is already on this org.")
    } else {
        format!("Email \"{email}\" is already mapped to \"{owner}\".")
    })
}

/// Reports a SQLite failure without leaking any query detail to the caller.
fn database_failure(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "organization persistence failed");
    AppError::Internal
}

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

/// `setColor(name, color)` — [lib/orgs.js:99-106]
///
/// # Errors
/// [`AppError::Validation`] for an unknown org or a non-hex color; [`AppError::Internal`] when
/// the update fails.
pub fn set_color(
    conn: &Connection,
    name: &str,
    color: Option<&str>,
) -> Result<Option<String>, AppError> {
    let name = norm_org(name);
    let color = js_trim(color.unwrap_or_default()).to_owned();
    if !org_exists(conn, &name)? {
        return Err(unknown_org(&name));
    }
    if !color.is_empty() && !is_valid_color(&color) {
        // [lib/orgs.js:103]
        return Err(AppError::Validation(
            "Color must be a hex value like #356B9F.".to_owned(),
        ));
    }
    let stored = (!color.is_empty()).then_some(color);
    conn.execute(
        "UPDATE orgs SET color = ? WHERE name = ?",
        rusqlite::params![stored, name],
    )
    .map_err(|error| database_failure("set org color", &error))?;
    Ok(stored)
}

/// `createOrg({ name, label, domain })` — [lib/orgs.js:108-127]
///
/// # Errors
/// [`AppError::Validation`] for a malformed/reserved/duplicate org id or a malformed/taken
/// domain; [`AppError::Internal`] when the transaction fails.
pub fn create_org(
    conn: &mut Connection,
    request: &CreateOrganization,
) -> Result<Organization, AppError> {
    // Case-folded on purpose: authorization compares org strings exactly, so accepting "Acme"
    // alongside "acme" would silently split one tenant in two. [lib/orgs.js:109-111]
    let name = js_trim(&request.name.0).to_lowercase();
    let label = truncate_utf16(js_trim(&request.label), ORG_LABEL_MAX_LENGTH);
    if !is_valid_org_name(&name) {
        // [lib/orgs.js:113]
        return Err(AppError::Validation(
            "Org name must be letters, numbers, dot, dash, or underscore (max 41).".to_owned(),
        ));
    }
    if is_valid_domain(&name) {
        return Err(AppError::Validation(
            "Org name must not be an email domain. Use a tenant id such as \"acme\" and add the domain separately."
                .to_owned(),
        ));
    }
    if name == RESERVED_ORG_NAME {
        // [lib/orgs.js:114]
        return Err(AppError::Validation(
            "\"admin\" is a reserved org name.".to_owned(),
        ));
    }
    if org_exists(conn, &name)? {
        // [lib/orgs.js:115]
        return Err(AppError::Validation(format!(
            "Organization \"{name}\" already exists."
        )));
    }
    // `const dom = domain ? normDomain(domain) : ""` — a missing, empty, or all-whitespace
    // domain is simply absent. [lib/orgs.js:116-117]
    let domain = request
        .domain
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(norm_domain)
        .filter(|value| !value.is_empty());
    if let Some(domain) = domain.as_deref() {
        if !is_valid_domain(domain) {
            return Err(invalid_domain(domain));
        }
        if let Some(owner) = domain_owner(conn, domain)? {
            // [lib/orgs.js:120]: the create path always reports the cross-org form.
            return Err(AppError::Validation(format!(
                "Domain \"{domain}\" is already mapped to \"{owner}\"."
            )));
        }
    }

    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database_failure("begin create org", &error))?;
    let organization = create_org_in_transaction(&transaction, &name, &label, domain.as_deref())?;
    transaction
        .commit()
        .map_err(|error| database_failure("commit create org", &error))?;
    Ok(organization)
}

fn create_org_in_transaction(
    transaction: &Transaction<'_>,
    name: &str,
    label: &str,
    domain: Option<&str>,
) -> Result<Organization, AppError> {
    insert_org(transaction, name, label)?;
    if let Some(domain) = domain {
        insert_domain(transaction, domain, name)?;
    }

    // Node returns a literal rather than re-reading the row, so `color` and `created_at` are
    // absent. [lib/orgs.js:126]
    Ok(Organization {
        name: OrgId(name.to_owned()),
        label: label.to_owned(),
        color: None,
        created_at: None,
        domains: domain
            .map(|value| vec![value.to_owned()])
            .unwrap_or_default(),
        emails: Vec::new(),
        categories: Vec::new(),
        key_count: 0,
    })
}

/// `deleteOrg(name)` — [lib/orgs.js:129-131]
///
/// Deletion is deliberately application-level because the frozen v21 schema has no foreign key
/// from artifacts or API keys to `orgs`. Owned artifacts refuse deletion so tenant content is not
/// orphaned or destroyed. With no artifacts, every active key is revoked in the same transaction
/// that deletes the registry row; domains, categories, explicit members, and webhooks then cascade.
///
/// # Errors
/// [`AppError::Validation`] while the org owns artifacts; [`AppError::Internal`] when the
/// transaction, lookup, key revocation, or delete fails.
pub fn delete_org(conn: &mut Connection, name: &str) -> Result<bool, AppError> {
    let name = norm_org(name);
    let transaction = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database_failure("begin delete org", &error))?;
    let result = delete_org_in_transaction(&transaction, &name)?;
    transaction
        .commit()
        .map_err(|error| database_failure("commit delete org", &error))?;
    Ok(result)
}

fn delete_org_in_transaction(transaction: &Transaction<'_>, name: &str) -> Result<bool, AppError> {
    if !org_exists(transaction, name)? {
        return Ok(false);
    }
    let artifact_count = transaction
        .query_row(
            "SELECT COUNT(*) FROM artifacts WHERE org = ?",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| database_failure("count org artifacts", &error))?;
    if artifact_count > 0 {
        let plural = if artifact_count == 1 { "" } else { "s" };
        return Err(AppError::Validation(format!(
            "Cannot delete organization \"{name}\" while it owns {artifact_count} artifact{plural}. Move its artifacts to another organization first."
        )));
    }
    transaction
        .execute(
            "UPDATE api_keys SET revoked_at = datetime('now') WHERE org = ? AND revoked_at IS NULL",
            [name],
        )
        .map_err(|error| database_failure("revoke org keys", &error))?;
    let removed = transaction
        .execute("DELETE FROM orgs WHERE name = ?", [name])
        .map_err(|error| database_failure("delete org", &error))?
        > 0;
    Ok(removed)
}

/// `addDomain(org, domain)` — [lib/orgs.js:133-144]
///
/// # Errors
/// [`AppError::Validation`] for an unknown org, malformed domain, or a domain already mapped;
/// [`AppError::Internal`] when the insert fails.
pub fn add_domain(conn: &Connection, org: &str, domain: &str) -> Result<String, AppError> {
    let org = norm_org(org);
    let domain = norm_domain(domain);
    if !org_exists(conn, &org)? {
        return Err(unknown_org(&org));
    }
    if !is_valid_domain(&domain) {
        return Err(invalid_domain(&domain));
    }
    if let Some(owner) = domain_owner(conn, &domain)? {
        return Err(domain_taken(&domain, &owner, owner == org));
    }
    insert_domain(conn, &domain, &org)?;
    Ok(domain)
}

/// `removeDomain(org, domain)` — [lib/orgs.js:146-148]
///
/// # Errors
/// [`AppError::Internal`] when the delete fails.
pub fn remove_domain(conn: &Connection, org: &str, domain: &str) -> Result<bool, AppError> {
    let org = norm_org(org);
    let domain = norm_domain(domain);
    if org == domain && domain_owner(conn, &domain)?.as_deref() == Some(org.as_str()) {
        return Err(AppError::Validation(format!(
            "Cannot remove domain \"{domain}\" from organization \"{org}\": implicit domain access would remain. Migrate to a non-domain organization first."
        )));
    }
    conn.execute(
        "DELETE FROM org_domains WHERE org = ? AND domain = ?",
        rusqlite::params![org, domain],
    )
    .map(|changes| changes > 0)
    .map_err(|error| database_failure("remove domain", &error))
}

/// `addEmailMember(org, email)` — [lib/orgs.js:150-163]
///
/// # Errors
/// [`AppError::Validation`] for an unknown org, invalid address, or an address already mapped;
/// [`AppError::Internal`] when the insert fails.
pub fn add_email_member(conn: &Connection, org: &str, email: &str) -> Result<String, AppError> {
    let org = norm_org(org);
    let email = norm_email(email);
    if !org_exists(conn, &org)? {
        return Err(unknown_org(&org));
    }
    if !is_valid_email(&email) {
        // [lib/orgs.js:154]
        return Err(AppError::Validation(format!(
            "\"{email}\" is not a valid email address."
        )));
    }
    if let Some(owner) = email_owner(conn, &email)? {
        return Err(email_taken(&email, &owner, owner == org));
    }
    conn.execute(
        "INSERT INTO org_email_members (email, org) VALUES (?, ?)",
        rusqlite::params![email, org],
    )
    .map_err(|error| database_failure("add email member", &error))?;
    Ok(email)
}

/// `removeEmailMember(org, email)` — [lib/orgs.js:165-167]
///
/// # Errors
/// [`AppError::Internal`] when the delete fails.
pub fn remove_email_member(conn: &Connection, org: &str, email: &str) -> Result<bool, AppError> {
    conn.execute(
        "DELETE FROM org_email_members WHERE org = ? AND email = ?",
        rusqlite::params![norm_org(org), norm_email(email)],
    )
    .map(|changes| changes > 0)
    .map_err(|error| database_failure("remove email member", &error))
}

/// `addCategory(org, name)` — [lib/orgs.js:169-176]
///
/// `INSERT OR IGNORE`, so re-adding an existing category succeeds, exactly as in Node.
///
/// # Errors
/// [`AppError::Validation`] for an unknown org or an empty category name;
/// [`AppError::Internal`] when the insert fails.
pub fn add_category(conn: &Connection, org: &str, name: &str) -> Result<String, AppError> {
    let org = norm_org(org);
    let category = norm_category(name);
    if !org_exists(conn, &org)? {
        return Err(unknown_org(&org));
    }
    if category.is_empty() {
        // [lib/orgs.js:173]
        return Err(AppError::Validation(
            "Category name is required.".to_owned(),
        ));
    }
    conn.execute(
        "INSERT OR IGNORE INTO org_categories (org, name) VALUES (?, ?)",
        rusqlite::params![org, category],
    )
    .map_err(|error| database_failure("add category", &error))?;
    Ok(category)
}

/// `removeCategory(org, name)` — [lib/orgs.js:178-180]
///
/// # Errors
/// [`AppError::Internal`] when the delete fails.
pub fn remove_category(conn: &Connection, org: &str, name: &str) -> Result<bool, AppError> {
    conn.execute(
        "DELETE FROM org_categories WHERE org = ? AND name = ?",
        rusqlite::params![norm_org(org), norm_category(name)],
    )
    .map(|changes| changes > 0)
    .map_err(|error| database_failure("remove category", &error))
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

fn domain_owner(conn: &Connection, domain: &str) -> Result<Option<String>, AppError> {
    conn.query_row(
        "SELECT org FROM org_domains WHERE domain = ?",
        [domain],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| database_failure("read domain owner", &error))
}

fn email_owner(conn: &Connection, email: &str) -> Result<Option<String>, AppError> {
    conn.query_row(
        "SELECT org FROM org_email_members WHERE email = ?",
        [email],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| database_failure("read email owner", &error))
}

fn insert_org(transaction: &Transaction<'_>, name: &str, label: &str) -> Result<(), AppError> {
    transaction
        .execute(
            "INSERT INTO orgs (name, label) VALUES (?, ?)",
            rusqlite::params![name, label],
        )
        .map(|_| ())
        .map_err(|error| database_failure("insert org", &error))
}

fn insert_domain(conn: &Connection, domain: &str, org: &str) -> Result<(), AppError> {
    conn.execute(
        "INSERT INTO org_domains (domain, org) VALUES (?, ?)",
        rusqlite::params![domain, org],
    )
    .map(|_| ())
    .map_err(|error| database_failure("insert org domain", &error))
}

// ---------------------------------------------------------------------------
// Pooled adapter
// ---------------------------------------------------------------------------

/// The `AdminService` organization surface backed by U03's pool.
///
/// `AdminService` is one frozen trait spanning keys, orgs, *and* webhooks (U12), so it cannot be
/// implemented piecewise. This store carries the org half under the frozen method names and
/// signatures; the composed adapter that implements the trait belongs to the unit that owns the
/// last piece.
#[derive(Clone)]
pub struct OrgStore {
    pool: DbPool,
}

impl OrgStore {
    /// Wrap a pool.
    #[must_use]
    pub const fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// See [`org_exists`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_exists(&self, org: &OrgId) -> Result<bool, AppError> {
        let org = org.0.clone();
        db::interact(&self.pool, move |conn| org_exists(conn, &org)).await
    }

    /// See [`org_for_domain`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_for_domain(&self, domain: &str) -> Result<Option<OrgId>, AppError> {
        let domain = domain.to_owned();
        db::interact(&self.pool, move |conn| org_for_domain(conn, &domain)).await
    }

    /// See [`org_for_email`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_for_email(&self, email: &EmailAddress) -> Result<Option<OrgId>, AppError> {
        let email = email.0.clone();
        db::interact(&self.pool, move |conn| org_for_email(conn, &email)).await
    }

    /// See [`org_names`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn org_names(&self) -> Result<Vec<OrgId>, AppError> {
        db::interact(&self.pool, |conn| org_names(conn)).await
    }

    /// See [`list_orgs`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn list_orgs(&self) -> Result<Vec<Organization>, AppError> {
        db::interact(&self.pool, |conn| list_orgs(conn)).await
    }

    /// See [`create_org`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn create_org(&self, request: CreateOrganization) -> Result<Organization, AppError> {
        db::interact(&self.pool, move |conn| create_org(conn, &request)).await
    }

    pub async fn create_org_audited(
        &self,
        request: CreateOrganization,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Organization, AppError> {
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let created = create_org_in_transaction_from_request(&tx, &request)?;
            let audit = audit.for_target_tenant(&created.name.0)?;
            let event = AuditEvent {
                operation: "org.create".to_owned(),
                target_type: "organization".to_owned(),
                target_id: created.name.0.clone(),
                result: "success".to_owned(),
                classification: "organization_created".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(created)
        })
        .await
    }

    /// See [`delete_org`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn delete_org(&self, org: &OrgId) -> Result<bool, AppError> {
        let org = org.0.clone();
        db::interact(&self.pool, move |conn| delete_org(conn, &org)).await
    }

    pub async fn delete_org_audited(
        &self,
        org: OrgId,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        db::interact(&self.pool, move |conn| {
            let target = norm_org(&org.0);
            let audit = audit.for_target_tenant(&target)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            if !delete_org_in_transaction(&tx, &target)? {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            }
            let event = AuditEvent {
                operation: "org.delete".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "organization_deleted".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(true)
        })
        .await
    }

    /// See [`add_domain`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn add_domain(&self, org: &OrgId, domain: &str) -> Result<String, AppError> {
        let (org, domain) = (org.0.clone(), domain.to_owned());
        db::interact(&self.pool, move |conn| add_domain(conn, &org, &domain)).await
    }

    pub async fn add_domain_audited(
        &self,
        org: OrgId,
        domain: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<String, AppError> {
        self.audited_org_value(
            org,
            domain,
            audit,
            audit_key,
            "org.domain.add",
            "domain_added",
            add_domain,
        )
        .await
    }

    /// See [`remove_domain`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn remove_domain(&self, org: &OrgId, domain: &str) -> Result<bool, AppError> {
        let (org, domain) = (org.0.clone(), domain.to_owned());
        db::interact(&self.pool, move |conn| remove_domain(conn, &org, &domain)).await
    }

    pub async fn remove_domain_audited(
        &self,
        org: OrgId,
        domain: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        self.audited_org_bool(
            org,
            domain,
            audit,
            audit_key,
            "org.domain.remove",
            "domain_removed",
            remove_domain,
        )
        .await
    }

    /// See [`add_email_member`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn add_email_member(
        &self,
        org: &OrgId,
        email: &EmailAddress,
    ) -> Result<EmailAddress, AppError> {
        let (org, email) = (org.0.clone(), email.0.clone());
        db::interact(&self.pool, move |conn| {
            add_email_member(conn, &org, &email).map(EmailAddress)
        })
        .await
    }

    pub async fn add_email_member_audited(
        &self,
        org: OrgId,
        email: EmailAddress,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<EmailAddress, AppError> {
        let value = self
            .audited_org_value(
                org,
                email.0,
                audit,
                audit_key,
                "org.member.add",
                "member_added",
                add_email_member,
            )
            .await?;
        Ok(EmailAddress(value))
    }

    /// See [`remove_email_member`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn remove_email_member(
        &self,
        org: &OrgId,
        email: &EmailAddress,
    ) -> Result<bool, AppError> {
        let (org, email) = (org.0.clone(), email.0.clone());
        db::interact(&self.pool, move |conn| {
            remove_email_member(conn, &org, &email)
        })
        .await
    }

    pub async fn remove_email_member_audited(
        &self,
        org: OrgId,
        email: EmailAddress,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        self.audited_org_bool(
            org,
            email.0,
            audit,
            audit_key,
            "org.member.remove",
            "member_removed",
            remove_email_member,
        )
        .await
    }

    /// See [`categories`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn categories(&self, org: &OrgId) -> Result<Vec<String>, AppError> {
        let org = org.0.clone();
        db::interact(&self.pool, move |conn| categories(conn, &org)).await
    }

    /// See [`add_category`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn add_category(&self, org: &OrgId, name: &str) -> Result<String, AppError> {
        let (org, name) = (org.0.clone(), name.to_owned());
        db::interact(&self.pool, move |conn| add_category(conn, &org, &name)).await
    }

    pub async fn add_category_audited(
        &self,
        org: OrgId,
        name: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<String, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        let pool = self.pool.clone();
        db::interact(&pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let normalized = norm_category(&name);
            let existed = tx
                .query_row(
                    "SELECT 1 FROM org_categories WHERE org=?1 AND name=?2",
                    (&org.0, &normalized),
                    |_| Ok(()),
                )
                .optional()
                .map_err(|error| database_failure("read category", &error))?
                .is_some();
            let result = add_category(&tx, &org.0, &name)?;
            if existed {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(result);
            }
            let event = AuditEvent {
                operation: "org.category.add".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "category_added".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(result)
        })
        .await
    }

    /// See [`remove_category`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn remove_category(&self, org: &OrgId, name: &str) -> Result<bool, AppError> {
        let (org, name) = (org.0.clone(), name.to_owned());
        db::interact(&self.pool, move |conn| remove_category(conn, &org, &name)).await
    }

    pub async fn remove_category_audited(
        &self,
        org: OrgId,
        name: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        self.audited_org_bool(
            org,
            name,
            audit,
            audit_key,
            "org.category.remove",
            "category_removed",
            remove_category,
        )
        .await
    }

    /// See [`color_map`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn color_map(&self) -> Result<BTreeMap<OrgId, Option<String>>, AppError> {
        db::interact(&self.pool, |conn| color_map(conn)).await
    }

    /// See [`set_color`].
    ///
    /// # Errors
    /// Propagates the synchronous operation's error.
    pub async fn set_color(
        &self,
        org: &OrgId,
        color: Option<&str>,
    ) -> Result<Option<String>, AppError> {
        let (org, color) = (org.0.clone(), color.map(str::to_owned));
        db::interact(&self.pool, move |conn| {
            set_color(conn, &org, color.as_deref())
        })
        .await
    }

    pub async fn set_color_audited(
        &self,
        org: OrgId,
        color: Option<String>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Option<String>, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let current = tx
                .query_row("SELECT color FROM orgs WHERE name=?1", [&org.0], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .optional()
                .map_err(|error| database_failure("read org color", &error))?;
            let stored = set_color(&tx, &org.0, color.as_deref())?;
            if current.as_ref() == Some(&stored) {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(stored);
            }
            let event = AuditEvent {
                operation: "org.color.set".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "color_updated".to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(stored)
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn audited_org_value(
        &self,
        org: OrgId,
        value: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
        operation: &'static str,
        classification: &'static str,
        mutation: fn(&Connection, &str, &str) -> Result<String, AppError>,
    ) -> Result<String, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        db::interact(&self.pool, move |conn| {
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                let result = mutation(tx, &org.0, &value)?;
                Ok((
                    result,
                    AuditEvent {
                        operation: operation.to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: target,
                        result: "success".to_owned(),
                        classification: classification.to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn audited_org_bool(
        &self,
        org: OrgId,
        value: String,
        audit: MutationAudit,
        audit_key: [u8; 32],
        operation: &'static str,
        classification: &'static str,
        mutation: fn(&Connection, &str, &str) -> Result<bool, AppError>,
    ) -> Result<bool, AppError> {
        let target = org.0.clone();
        let audit = audit.for_target_tenant(&target)?;
        db::interact(&self.pool, move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            if !mutation(&tx, &org.0, &value)? {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            }
            let event = AuditEvent {
                operation: operation.to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: classification.to_owned(),
                revision: None,
            };
            crate::security::audit::append_in_transaction(
                &tx,
                &audit_key,
                &audit.event_id()?,
                audit.context(),
                &event,
            )?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(true)
        })
        .await
    }
}

fn create_org_in_transaction_from_request(
    tx: &Transaction<'_>,
    request: &CreateOrganization,
) -> Result<Organization, AppError> {
    let name = js_trim(&request.name.0).to_lowercase();
    let label = truncate_utf16(js_trim(&request.label), ORG_LABEL_MAX_LENGTH);
    if !is_valid_org_name(&name) {
        return Err(AppError::Validation(
            "Org name must be letters, numbers, dot, dash, or underscore (max 41).".to_owned(),
        ));
    }
    if is_valid_domain(&name) {
        return Err(AppError::Validation("Org name must not be an email domain. Use a tenant id such as \"acme\" and add the domain separately.".to_owned()));
    }
    if name == RESERVED_ORG_NAME {
        return Err(AppError::Validation(
            "\"admin\" is a reserved org name.".to_owned(),
        ));
    }
    if org_exists(tx, &name)? {
        return Err(AppError::Validation(format!(
            "Organization \"{name}\" already exists."
        )));
    }
    let domain = request
        .domain
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(norm_domain)
        .filter(|value| !value.is_empty());
    if let Some(domain) = domain.as_deref() {
        if !is_valid_domain(domain) {
            return Err(invalid_domain(domain));
        }
        if let Some(owner) = domain_owner(tx, domain)? {
            return Err(AppError::Validation(format!(
                "Domain \"{domain}\" is already mapped to \"{owner}\"."
            )));
        }
    }
    create_org_in_transaction(tx, &name, &label, domain.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_the_javascript_whitespace_set_not_rusts() {
        // U+FEFF is JS whitespace but not Unicode `White_Space`; U+0085 is the reverse.
        assert_eq!(js_trim("\u{feff} acme \u{feff}"), "acme");
        assert_eq!(js_trim("\u{85}acme"), "\u{85}acme");
        assert_eq!(js_trim("\u{3000}acme\u{2029}"), "acme");
    }

    #[test]
    fn measures_and_truncates_in_utf16_code_units() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(truncate_utf16("😀😀", 3), "😀");
        assert_eq!(truncate_utf16("abc", 10), "abc");
    }

    #[test]
    fn accepts_only_node_shaped_org_names() {
        assert!(is_valid_org_name("a"));
        assert!(is_valid_org_name("A-c.m_e0"));
        assert!(is_valid_org_name(&format!("a{}", "b".repeat(40))));
        assert!(!is_valid_org_name(&format!("a{}", "b".repeat(41))));
        assert!(!is_valid_org_name(""));
        assert!(!is_valid_org_name("-acme"));
        assert!(!is_valid_org_name("ac me"));
        assert!(!is_valid_org_name("acme\n"));
    }

    #[test]
    fn requires_at_least_one_dot_in_a_domain() {
        assert!(is_valid_domain("example.com"));
        assert!(is_valid_domain("a.b"));
        assert!(is_valid_domain("sub.example.co.uk"));
        assert!(is_valid_domain("x-y.example"));
        assert!(!is_valid_domain("example"));
        assert!(!is_valid_domain("-bad.example"));
        assert!(!is_valid_domain("bad-.example"));
        assert!(!is_valid_domain("bad..example"));
        assert!(!is_valid_domain(".example.com"));
        assert!(!is_valid_domain("example.com."));
    }

    #[test]
    fn matches_node_email_validation() {
        assert!(is_valid_email("a@b.c"));
        assert!(is_valid_email("first.last+tag@example.co.uk"));
        assert!(!is_valid_email("@example.com"));
        assert!(!is_valid_email("a@@example.com"));
        assert!(!is_valid_email("a b@example.com"));
        assert!(!is_valid_email(".a@example.com"));
        assert!(!is_valid_email("a.@example.com"));
        assert!(!is_valid_email("a..b@example.com"));
        assert!(!is_valid_email("a@example"));
        assert!(!is_valid_email(""));
        assert!(!is_valid_email(&format!("{}@example.com", "a".repeat(65))));
    }

    #[test]
    fn collapses_and_caps_category_names() {
        assert_eq!(norm_category("  design   docs \n"), "design docs");
        assert_eq!(norm_category("\u{feff}"), "");
        assert_eq!(norm_category(&"x".repeat(70)).len(), CATEGORY_MAX_LENGTH);
    }

    #[test]
    fn accepts_three_and_six_digit_hex_colors() {
        assert!(is_valid_color("#abc"));
        assert!(is_valid_color("#356B9F"));
        assert!(!is_valid_color("356B9F"));
        assert!(!is_valid_color("#abcd"));
        assert!(!is_valid_color("#gggggg"));
    }
}
