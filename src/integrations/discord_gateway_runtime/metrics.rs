//! Process-wide Discord inbound counters and their Prometheus rendering.

use std::sync::{LazyLock, Mutex};

use crate::persistence::discord_inbound::DiscordInboundMetricsSnapshot;

#[derive(Default)]
pub(super) struct DiscordInboundProcessMetrics {
    snapshot: DiscordInboundMetricsSnapshot,
    pub(super) reconnects: u64,
    pub(super) duplicates: u64,
    pub(super) application_errors: u64,
}

static INBOUND_METRICS: LazyLock<Mutex<DiscordInboundProcessMetrics>> =
    LazyLock::new(|| Mutex::new(DiscordInboundProcessMetrics::default()));

pub fn render_prometheus() -> String {
    let metrics = INBOUND_METRICS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let snapshot = &metrics.snapshot;
    format!(
        "# HELP artifact_mcp_discord_gateway_organizations Organization Gateway sessions by fixed health state.\n\
         # TYPE artifact_mcp_discord_gateway_organizations gauge\n\
         artifact_mcp_discord_gateway_organizations{{state=\"disabled\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"connecting\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"ready\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"reconnecting\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"degraded\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"failed\"}} {}\n\
         # HELP artifact_mcp_discord_gateway_reconnects_total Gateway reconnect signals observed by this process.\n\
         # TYPE artifact_mcp_discord_gateway_reconnects_total counter\n\
         artifact_mcp_discord_gateway_reconnects_total {}\n\
         # HELP artifact_mcp_discord_inbound_inbox_depth Retained body-free inbound event receipts.\n\
         # TYPE artifact_mcp_discord_inbound_inbox_depth gauge\n\
         artifact_mcp_discord_inbound_inbox_depth {}\n\
         # HELP artifact_mcp_discord_inbound_pending_fetches Partial updates awaiting REST hydration.\n\
         # TYPE artifact_mcp_discord_inbound_pending_fetches gauge\n\
         artifact_mcp_discord_inbound_pending_fetches {}\n\
         # HELP artifact_mcp_discord_inbound_last_event_age_seconds Age of the newest retained inbound receipt.\n\
         # TYPE artifact_mcp_discord_inbound_last_event_age_seconds gauge\n\
         artifact_mcp_discord_inbound_last_event_age_seconds {}\n\
         # HELP artifact_mcp_discord_inbound_oldest_pending_age_seconds Age of the oldest deferred update.\n\
         # TYPE artifact_mcp_discord_inbound_oldest_pending_age_seconds gauge\n\
         artifact_mcp_discord_inbound_oldest_pending_age_seconds {}\n\
         # HELP artifact_mcp_discord_inbound_ignored Retained ignored inbound events.\n\
         # TYPE artifact_mcp_discord_inbound_ignored gauge\n\
         artifact_mcp_discord_inbound_ignored {}\n\
         # HELP artifact_mcp_discord_inbound_rejected_or_degraded Retained rejected, degraded, or failed inbound events.\n\
         # TYPE artifact_mcp_discord_inbound_rejected_or_degraded gauge\n\
         artifact_mcp_discord_inbound_rejected_or_degraded {}\n\
         # HELP artifact_mcp_discord_inbound_tombstones Discord-origin feedback tombstones.\n\
         # TYPE artifact_mcp_discord_inbound_tombstones gauge\n\
         artifact_mcp_discord_inbound_tombstones {}\n\
         # HELP artifact_mcp_discord_inbound_duplicates_total Replay duplicates observed by this process.\n\
         # TYPE artifact_mcp_discord_inbound_duplicates_total counter\n\
         artifact_mcp_discord_inbound_duplicates_total {}\n\
         # HELP artifact_mcp_discord_inbound_application_errors_total Inbound events that failed before a durable result.\n\
         # TYPE artifact_mcp_discord_inbound_application_errors_total counter\n\
         artifact_mcp_discord_inbound_application_errors_total {}\n",
        snapshot.gateway_disabled,
        snapshot.gateway_connecting,
        snapshot.gateway_ready,
        snapshot.gateway_reconnecting,
        snapshot.gateway_degraded,
        snapshot.gateway_failed,
        metrics.reconnects,
        snapshot.inbox_depth,
        snapshot.pending_fetches,
        snapshot.last_event_age_seconds,
        snapshot.oldest_pending_age_seconds,
        snapshot.ignored_events,
        snapshot.rejected_or_degraded_events,
        snapshot.tombstones,
        metrics.duplicates,
        metrics.application_errors,
    )
}

pub(super) fn refresh_metrics(snapshot: DiscordInboundMetricsSnapshot) {
    INBOUND_METRICS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .snapshot = snapshot;
}

pub(super) fn increment_metric(select: fn(&mut DiscordInboundProcessMetrics) -> &mut u64) {
    let mut metrics = INBOUND_METRICS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let value = select(&mut metrics);
    *value = value.saturating_add(1);
}
