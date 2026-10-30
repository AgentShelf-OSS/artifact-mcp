use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Mutex,
};

use super::*;

/// Scriptable fake Gateway: records Identify/Resume/Heartbeat/Reconnect/Shutdown in exact order.
#[derive(Default)]
pub struct FakeDiscordGateway {
    frames: Mutex<VecDeque<GatewayFrame>>,
    commands: Mutex<Vec<GatewayCommand>>,
}

impl FakeDiscordGateway {
    #[must_use]
    pub fn scripted(frames: impl IntoIterator<Item = GatewayFrame>) -> Self {
        Self {
            frames: Mutex::new(frames.into_iter().collect()),
            commands: Mutex::new(Vec::new()),
        }
    }
    #[must_use]
    pub fn commands(&self) -> Vec<GatewayCommand> {
        self.commands.lock().expect("fake gateway mutex").clone()
    }
}
impl DiscordGatewayPort for FakeDiscordGateway {
    fn next_frame(&self) -> Option<GatewayFrame> {
        self.frames.lock().expect("fake gateway mutex").pop_front()
    }
    fn send(&self, command: GatewayCommand) {
        self.commands
            .lock()
            .expect("fake gateway mutex")
            .push(command);
    }
}

/// Deterministic fake REST source used by partial-update and rate-limit tests.
type DiscordMessageKey = (String, String, String);
type FakeRestReply = Result<DiscordMessage, RestError>;

#[derive(Default)]
pub struct FakeDiscordRest {
    messages: Mutex<BTreeMap<DiscordMessageKey, FakeRestReply>>,
}
impl FakeDiscordRest {
    pub fn put(&self, message: DiscordMessage) {
        self.messages.lock().expect("fake rest mutex").insert(
            (
                message.guild_id.clone(),
                message.thread_id.clone(),
                message.id.clone(),
            ),
            Ok(message),
        );
    }
    pub fn fail(&self, guild: &str, thread: &str, message: &str, error: RestError) {
        self.messages
            .lock()
            .expect("fake rest mutex")
            .insert((guild.into(), thread.into(), message.into()), Err(error));
    }
}
impl DiscordRestPort for FakeDiscordRest {
    fn fetch_message(
        &self,
        guild: &str,
        thread: &str,
        message: &str,
    ) -> Result<Option<DiscordMessage>, RestError> {
        match self
            .messages
            .lock()
            .expect("fake rest mutex")
            .get(&(guild.into(), thread.into(), message.into()))
            .cloned()
        {
            Some(result) => result.map(Some),
            None => Ok(None),
        }
    }
}

/// In-memory transactional-shaped fake.  It retains fingerprints/results only, not raw content.
#[derive(Default)]
pub struct MemoryInbox {
    events: BTreeSet<(String, String, String)>,
    pub results: Vec<(String, InboundResult)>,
    pub feedback: BTreeMap<String, InboundFeedback>,
    pub degraded: Vec<ThreadDegradedReason>,
}
impl InboundEventInbox for MemoryInbox {
    fn has_event(&self, event: &InboundEvent) -> bool {
        self.events.contains(&(
            event.org.0.clone(),
            event.gateway_session_id.clone(),
            event.event_id.clone(),
        ))
    }
    fn record_event(&mut self, event: &InboundEvent, result: InboundResult) {
        self.events.insert((
            event.org.0.clone(),
            event.gateway_session_id.clone(),
            event.event_id.clone(),
        ));
        self.results
            .push((event.payload_fingerprint.clone(), result));
    }
    fn find_message(&self, external_message_id: &str) -> Option<&InboundFeedback> {
        self.feedback.get(external_message_id)
    }
    fn put_feedback(&mut self, feedback: InboundFeedback) {
        self.feedback
            .insert(feedback.external_message_id.clone(), feedback);
    }
    fn mark_thread_degraded(&mut self, reason: ThreadDegradedReason) {
        self.degraded.push(reason);
    }
}
