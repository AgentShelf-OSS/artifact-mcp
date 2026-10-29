//! Owned by U11 (terra) — feedback threads, anchors, and ownership mutations.
//!
//! Node oracle: `lib/feedback.js` (whole file) plus the two validators the HTTP route applies
//! before it — `validateAnchorPage` [lib/app.js:13-36] and the ownership pre-checks at
//! [lib/app.js:604-613] / [lib/app.js:617-626].
//!
//! # What is reproduced exactly
//!
//! * **Error strings and their evaluation order.** `addFeedback` checks the body, then the reply
//!   parent, and only then normalizes the anchor — because `normalizeAnchor` is invoked inside the
//!   object literal handed to `insertStmt.run` [lib/feedback.js:107], which JavaScript evaluates
//!   after both guards have already thrown. A submission that is empty *and* carries a broken
//!   anchor reports the empty-body message in both runtimes.
//! * **JavaScript string semantics.** `String.prototype.trim` and `String.prototype.length` are
//!   not Rust's: JS trims `U+FEFF` but not `U+0085`, and counts UTF-16 code units, so one emoji
//!   costs two characters of `FEEDBACK_MAX_BODY`. See [`js_trim`] and [`utf16_len`].
//! * **SQL result ordering.** Every listing keeps Node's `ORDER BY` verbatim: unresolved before
//!   resolved, then the per-listing tiebreak. Later HTTP/MCP conformance compares ordered JSON
//!   arrays, so a reordering is a visible break.
//! * **Thread cascade.** `parent_id TEXT REFERENCES feedback(id) ON DELETE CASCADE` (migration 13)
//!   deletes replies with their parent — which only holds because `foreign_keys = ON` is one of
//!   the six pragmas U03 pins and verifies on every connection.
//!
//! # Scope checks live here, not only in the route
//!
//! Node's viewer mutations are guarded twice: the route rejects a feedback row belonging to a
//! different artifact or org [lib/app.js:610], then the store rejects a different viewer
//! [lib/feedback.js:138]. [`delete_as_viewer`] and [`resolve_as_viewer`] take the artifact scope
//! as an argument and apply both, in Node's order, so a route cannot lose half of the check and
//! turn a feedback id into a cross-tenant oracle.

mod anchors;
mod reads;
mod sql;
mod writes;

pub use anchors::*;
pub use reads::*;
pub use writes::*;
