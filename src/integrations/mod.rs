//! Integration declarations frozen by U01; implementations are owned by U12 and U16.

pub mod delivery_envelope;
pub mod delivery_runtime;
pub mod delivery_worker;
/// Provider-neutral-in-spirit Discord webhook protocol used by the durable-delivery worker.
///
/// This is intentionally separate from `notify`: PBI-056's worker cutover must not change the
/// existing detached notifier or the awaited administrator webhook test.
pub mod discord_delivery;
/// Provider-neutral discussion operations with Discord's forum/media-webhook implementation.
///
/// This slice deliberately owns no persistence or outbox orchestration.  PBI-079's later
/// application layer supplies the durable mapping and ordering around these bounded requests.
pub mod discord_discussion;
pub mod discord_gateway_runtime;
pub mod discord_history_recovery;
/// PBI-080 inbound contracts, fake provider, and processor.
pub mod discord_inbound;
pub mod discord_recovery_runtime;
pub mod discussion_envelope;
pub mod notify;
pub mod preview;
pub mod preview_notifier;
pub mod thumbnails;
