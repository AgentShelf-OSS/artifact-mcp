//! Production implementations of the application ports.
//!
//! Each adapter wires one port trait to the SQLite-backed stores and integrations. The binary
//! (`src/main.rs`) constructs them during startup and passes them to the router as `AppDeps`.

mod admin;
mod directories;
mod discussions;
mod engagement;
mod health;
mod preview_index;
mod shares;

pub use admin::ProductionAdmin;
pub use directories::ProductionDirectories;
pub use discussions::ProductionDiscussions;
pub use engagement::ProductionEngagement;
pub use health::ProductionHealth;
pub use preview_index::StartupPreviewIndex;
pub use shares::ProductionShares;
