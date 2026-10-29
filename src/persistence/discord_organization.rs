//! PBI-081 organization-scoped Discord credentials, inherited policy, and recovery evidence.
//!
//! This store deliberately has no HTTP or Discord dependency.  A route/provider validates a
//! proposed token *before* calling [`OrganizationDiscordStore::save_validated_credential`], so a
//! failed external validation cannot replace a working credential while a SQLite transaction is
//! open.  The only secret-bearing return type is the server-side [`Secret`](crate::config::Secret) used by providers.
//!
//! The implementation is grouped into sibling modules: credentials and policy, recovery, and
//! shared helpers and types.

mod credentials;
mod helpers;
mod recovery;
mod types;

pub use types::*;
