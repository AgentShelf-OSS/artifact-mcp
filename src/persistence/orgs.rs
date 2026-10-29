//! Owned by U09 (terra) — organization, membership, category, and color persistence.
//!
//! Node oracle: `lib/orgs.js` in full. Every validation rule, normalization step, and error
//! string below is a faithful port; the admin routes turn each thrown `Error` into
//! `400 {"error": message}` ([lib/app.js:308-310], [lib/app.js:329-331], [lib/app.js:348-350],
//! [lib/app.js:369-371], [lib/app.js:387-389]), so a message that differs by one character is a
//! user-visible divergence. Thrown errors therefore map to [`crate::error::AppError::Validation`], whose HTTP
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
