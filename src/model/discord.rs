//! Discord provider values shared by persistence and integrations.

/// A normalized Discord author.  Its id/display are opaque provider identifiers, never emails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordAuthor {
    pub id: String,
    pub display: String,
    pub is_bot: bool,
    pub webhook_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IgnoreReason {
    Disabled,
    Unmapped,
    Bot,
    Webhook,
    Unsupported,
    EmptyContent,
    Stale,
    UnknownMessage,
}

/// Optional integration health, deliberately distinct from process liveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayHealth {
    Disabled,
    Connecting,
    Ready,
    Reconnecting,
    Degraded,
    Failed,
    Shutdown,
}
