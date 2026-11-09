use std::collections::{BTreeMap, BTreeSet};

use super::{
    InboundEvent, InboundEventInbox, InboundFeedback, InboundResult, ThreadDegradedReason,
};

/// In-memory processing state used by the production persistence adapter within a transaction.
/// Event receipts retain fingerprints/results, not raw provider payloads; feedback holds the
/// current message state loaded and persisted by the adapter.
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
