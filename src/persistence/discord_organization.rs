//! PBI-081 organization-scoped Discord credentials, inherited policy, and recovery evidence.
//!
//! This facade preserves the original public module paths. Implementations are grouped by
//! credential/policy and recovery responsibilities in sibling modules.

mod credentials;
mod helpers;
mod recovery;
mod types;

pub use types::*;
