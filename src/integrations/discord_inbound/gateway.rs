use super::*;

/// Gateway commands capture official lifecycle constraints without a networking dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayCommand {
    Identify { intents: u64 },
    Resume { session_id: String, sequence: u64 },
    Heartbeat { sequence: Option<u64> },
    Reconnect,
    Shutdown,
}

/// Readiness is an optional integration signal. Missing intents or permissions block only inbound
/// activation; Artifact MCP's local feedback/liveness stays available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscordReadiness {
    Ready,
    MissingCredential,
    MissingMessageContentIntent,
    MissingGuildAccess,
    MissingThreadPermission,
    InvalidOrDisallowedIntents,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GatewayResumeState {
    pub session_id: Option<String>,
    pub resume_gateway_url: Option<String>,
    pub last_sequence: Option<u64>,
    pub awaiting_heartbeat_ack: bool,
}

/// Pure official-protocol lifecycle model. It has no socket, timer, credential, or app-liveness
/// ownership; deterministic tests use it to pin the behavior delegated to the maintained client.
#[derive(Clone, Debug)]
pub struct GatewayLifecycle {
    intents: u64,
    pub state: GatewayResumeState,
}

impl GatewayLifecycle {
    #[must_use]
    pub const fn new(intents: u64) -> Self {
        Self {
            intents,
            state: GatewayResumeState {
                session_id: None,
                resume_gateway_url: None,
                last_sequence: None,
                awaiting_heartbeat_ack: false,
            },
        }
    }

    #[must_use]
    pub const fn readiness(
        credential_present: bool,
        message_content_enabled: bool,
        guild_access: bool,
        thread_permission: bool,
        intent_valid: bool,
    ) -> DiscordReadiness {
        if !credential_present {
            DiscordReadiness::MissingCredential
        } else if !intent_valid {
            DiscordReadiness::InvalidOrDisallowedIntents
        } else if !message_content_enabled {
            DiscordReadiness::MissingMessageContentIntent
        } else if !guild_access {
            DiscordReadiness::MissingGuildAccess
        } else if !thread_permission {
            DiscordReadiness::MissingThreadPermission
        } else {
            DiscordReadiness::Ready
        }
    }

    /// READY supplies resume state; a non-resumable INVALID_SESSION clears it and re-identifies.
    #[must_use]
    pub fn on_frame(&mut self, frame: &GatewayFrame) -> Option<GatewayCommand> {
        match frame {
            GatewayFrame::Hello { .. } => {
                match (&self.state.session_id, self.state.last_sequence) {
                    (Some(session_id), Some(sequence)) => Some(GatewayCommand::Resume {
                        session_id: session_id.clone(),
                        sequence,
                    }),
                    _ => Some(GatewayCommand::Identify {
                        intents: self.intents,
                    }),
                }
            }
            GatewayFrame::Ready {
                session_id,
                resume_gateway_url,
            } => {
                self.state.session_id = Some(session_id.clone());
                self.state.resume_gateway_url = Some(resume_gateway_url.clone());
                None
            }
            GatewayFrame::Dispatch { sequence, .. } => {
                self.state.last_sequence = Some(*sequence);
                None
            }
            GatewayFrame::HeartbeatAck => {
                self.state.awaiting_heartbeat_ack = false;
                None
            }
            GatewayFrame::Reconnect => Some(GatewayCommand::Reconnect),
            GatewayFrame::InvalidSession { resumable: false } => {
                self.state = GatewayResumeState::default();
                Some(GatewayCommand::Identify {
                    intents: self.intents,
                })
            }
            GatewayFrame::InvalidSession { resumable: true } => Some(GatewayCommand::Reconnect),
            GatewayFrame::RateLimited { .. } | GatewayFrame::Closed => None,
        }
    }

    /// A missed ACK requires reconnect; otherwise heartbeat carries the last dispatch sequence.
    #[must_use]
    pub fn heartbeat_due(&mut self) -> GatewayCommand {
        if self.state.awaiting_heartbeat_ack {
            GatewayCommand::Reconnect
        } else {
            self.state.awaiting_heartbeat_ack = true;
            GatewayCommand::Heartbeat {
                sequence: self.state.last_sequence,
            }
        }
    }
}

/// Deterministic frame subset for testing a future maintained client adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatewayFrame {
    Hello {
        heartbeat_interval_ms: u64,
    },
    Ready {
        session_id: String,
        resume_gateway_url: String,
    },
    Dispatch {
        sequence: u64,
        event: Box<InboundEvent>,
    },
    HeartbeatAck,
    Reconnect,
    InvalidSession {
        resumable: bool,
    },
    RateLimited {
        retry_after_ms: u64,
    },
    Closed,
}

/// Provider-neutral Gateway test port. Production uses `twilight-gateway`; this contract keeps
/// deterministic lifecycle scenarios free of sockets, credentials, and process ownership.
pub trait DiscordGatewayPort: Send + Sync {
    fn next_frame(&self) -> Option<GatewayFrame>;
    fn send(&self, command: GatewayCommand);
}
