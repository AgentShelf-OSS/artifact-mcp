//! Durable provider-delivery storage; this module never performs network I/O.
//!
//! A durability intent keeps a row `blocked` until the producer's filesystem work succeeds.
//! Worker restart is bounded at-least-once: an expired lease is retryable and records duplicate
//! risk instead of silently claiming an outcome.

mod claim;
mod enqueue;
mod model;
mod status;
mod support;
mod transition;
mod validation;

pub use claim::claim_next;
pub use enqueue::{
    compensate_durability_in_transaction, enqueue_in_transaction, enqueue_many_in_transaction,
    finalize_durability_success_in_transaction,
};
pub use model::*;
pub use status::queue_status;
pub use transition::{
    accepted, accepted_in_transaction, dead_letter, dead_letter_in_transaction, persist_rate_limit,
    retry, verify_payload_hash,
};

pub(crate) use support::{find_event, get, hash, internal, row};
pub(crate) use validation::{validate, validate_dependency_contract};
