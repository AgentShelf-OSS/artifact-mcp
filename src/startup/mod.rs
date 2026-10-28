//! Startup stages for the production binary.
//!
//! `src/main.rs` runs these stages in a fixed order and emits its startup observer stages
//! between them:
//!
//! 1. [`database`]: open and migrate SQLite, report pragmas, seed publisher keys.
//! 2. [`storage`]: open the artifact store, reconcile storage, warm the preview cache.
//! 3. [`identity`]: publisher authentication and viewer identity.
//! 4. [`workers`]: integration stores, the detached notifier, and the background runtimes.
//! 5. [`services`]: the production port adapters.

pub mod database;
pub mod identity;
pub mod services;
pub mod storage;
pub mod workers;

pub use database::{
    audit_ledger_key, log_effective_pragmas, open_database, seed_publisher_keys, webhook_protection,
};
pub use identity::{Authentication, authentication};
pub use services::{PortServices, port_services};
pub use storage::{ContentStores, open_content, reconcile_storage, warm_previews};
pub use workers::{
    BackgroundRuntimes, IntegrationStores, StartedWorkers, notification_sink,
    start_background_workers,
};
