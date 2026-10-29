use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior};

use super::queries::{domain_owner, email_owner};
use super::semantics::{
    ORG_LABEL_MAX_LENGTH, RESERVED_ORG_NAME, database_failure, domain_taken, email_taken,
    invalid_domain, is_valid_email, is_valid_org_name, js_trim, norm_category, norm_domain,
    norm_email, norm_org, truncate_utf16, unknown_org,
};
use super::{is_valid_color, is_valid_domain, org_exists};
use crate::error::AppError;
use crate::model::{CreateOrganization, OrgId, Organization};

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

pub(super) fn delete_org_in_transaction(
    transaction: &Transaction<'_>,
    name: &str,
) -> Result<bool, AppError> {
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

/// Add a member or update its display name without moving it between organizations.
pub fn add_email_member_with_name(
    conn: &Connection,
    org: &str,
    email: &str,
    display_name: Option<&str>,
) -> Result<String, AppError> {
    let Some(display_name) = display_name else {
        return add_email_member(conn, org, email);
    };
    let org = norm_org(org);
    let email = norm_email(email);
    if !org_exists(conn, &org)? {
        return Err(unknown_org(&org));
    }
    if !is_valid_email(&email) {
        return Err(AppError::Validation(format!(
            "\"{email}\" is not a valid email address."
        )));
    }
    let display_name = normalize_display_name(display_name)?;
    if let Some(owner) = email_owner(conn, &email)?
        && owner != org
    {
        return Err(email_taken(&email, &owner, false));
    }
    conn.execute(
        "INSERT INTO org_email_members (email,org,display_name) VALUES (?,?,?) ON CONFLICT(email) DO UPDATE SET display_name=excluded.display_name WHERE org_email_members.org=excluded.org",
        rusqlite::params![email, org, display_name],
    ).map_err(|error| database_failure("upsert email member", &error))?;
    Ok(email)
}

pub fn normalize_display_name(value: &str) -> Result<String, AppError> {
    if value.chars().any(char::is_control) || js_trim(value).chars().count() > 40 {
        return Err(AppError::Validation(
            "display_name must be a single line of at most 40 characters.".into(),
        ));
    }
    Ok(js_trim(value).to_owned())
}

pub fn email_display_name(conn: &Connection, email: &str) -> Result<Option<String>, AppError> {
    conn.query_row(
        "SELECT display_name FROM org_email_members WHERE email = ?",
        [norm_email(email)],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map(|v| v.filter(|s| !s.is_empty()))
    .map_err(|error| database_failure("read email display name", &error))
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

pub(super) fn create_org_in_transaction_from_request(
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
