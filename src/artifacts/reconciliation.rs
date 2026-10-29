//! Storage reconciliation and digest backfill.
//!
//! This facade preserves the established `artifacts::reconciliation` public paths. The
//! implementation is separated by responsibility in sibling child modules.

mod audit;
mod backfill;
mod intents;
mod support;

pub use audit::audit_storage;
pub use backfill::backfill_body_digests;
