//! Owned by U01 (sol) — frozen object-safe application ports.

use std::{future::Future, pin::Pin};

pub mod admin;
pub mod artifacts;
pub mod engagement;
pub mod identity;
pub mod integrations;
pub mod rendering;

pub use admin::AdminService;
pub use artifacts::ArtifactService;
pub use engagement::{EngagementService, ShareService};
pub use identity::{PublisherAuthenticator, ViewerIdentity};
pub use integrations::{HealthProbe, NotificationSink, PreviewService};
pub use rendering::PageRenderer;

/// Object-safe asynchronous return type used instead of an async-trait macro.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
