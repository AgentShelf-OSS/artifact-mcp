//! Persistence declarations frozen by U01; adapters are owned by U03 and U09–U12.

pub mod db;
pub mod discord_inbound;
pub mod discord_organization;
pub mod discussions;
pub mod feedback;
pub mod feedback_delivery;
pub mod keys;
pub mod migrations;
pub mod notifications;
pub mod orgs;
pub mod outbox;
pub mod outbox_fanout;
pub mod reactions;
pub mod shares;
pub mod views;
pub mod webhooks;
