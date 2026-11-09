//! Feedback threads, anchors, and ownership mutations.
//!
//! HTTP routes validate anchor pages and artifact ownership before calling this store.
//!
//! # What is reproduced exactly
//!
//! * **Error strings and their evaluation order.** Check the body, then the reply parent, and only then normalize the anchor. The retired Node twin normalized the anchor while building the insert arguments, after both guards had passed. A submission that is empty *and* carries a broken anchor must therefore report the frozen empty-body message.
//! * **JavaScript string semantics.** `String.prototype.trim` and `String.prototype.length` are not Rust's: JS trims `U+FEFF` but not `U+0085`, and counts UTF-16 code units, so one emoji costs two characters of `FEEDBACK_MAX_BODY`. See [`js_trim`] and [`utf16_len`].
//! * **SQL result ordering.** Every listing keeps the frozen `ORDER BY`: unresolved before resolved, then the per-listing tiebreak. HTTP/MCP conformance compares ordered JSON arrays, so a reordering is a visible break.
//! * **Thread cascade.** `parent_id TEXT REFERENCES feedback(id) ON DELETE CASCADE` (migration 13) deletes replies with their parent — which only holds because `foreign_keys = ON` is one of the six pragmas verified on every connection.
//!
//! # Scope checks live here, not only in the route
//!
//! Viewer mutations are guarded twice: reject a feedback row belonging to a different artifact or org, then reject a different viewer. [`delete_as_viewer`] and [`resolve_as_viewer`] take the artifact scope as an argument and apply both checks in that order, so a route cannot lose half of the check and turn a feedback id into a cross-tenant oracle.

mod anchors;
mod reads;
mod sql;
mod writes;

pub use anchors::*;
pub use reads::*;
pub use writes::*;
