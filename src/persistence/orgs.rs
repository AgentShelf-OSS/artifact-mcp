//! Organization, membership, category, and color persistence.
//!
//! Validation messages reach admin callers verbatim as `400 {"error": message}`. A one-character difference is visible, so validation errors map to [`crate::error::AppError::Validation`] with the supplied message.
//!
//! # Ordering is part of the contract
//!
//! Check preconditions in a fixed order and return on the first failure, because the *message* a caller sees depends on that order. For example, adding a category reports the unknown org before it reports an empty category name. The sync operations below preserve that frozen sequence.
//!
//! # Case handling
//!
//! * Org ids are case-folded on create but matched *exactly* everywhere else (`orgExists` only trims), because migration v7 can seed mixed-case orgs from `api_keys`/`artifacts`.
//! * Domains and emails are trimmed and lowercased.
//! * The v21 `org_email_members.email` column is `COLLATE NOCASE`, so membership lookups and deletes are case-insensitive *in SQLite as well* — the normalization and the collation are two independent layers and both are preserved.
//!
//! # Deliberate divergence (contract-delta request)
//!
//! `String.prototype.slice(0, n)` counts UTF-16 code units and will happily cut a surrogate pair in half; better-sqlite3 then stores the lone surrogate as the invalid UTF-8 sequence `ED A0 BD` (verified empirically). A Rust `String` cannot hold that, so [`truncate_utf16`] drops the straddling character instead. Only reachable with an astral character that starts at exactly the label/category cap.

mod mutations;
mod queries;
mod semantics;
mod store;

pub use mutations::*;
pub use queries::*;
pub use semantics::*;
pub use store::OrgStore;

#[cfg(test)]
mod tests;
