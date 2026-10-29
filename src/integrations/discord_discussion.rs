//! Discord discussion transport facade.

mod request;
mod transport;

pub use request::{
    DiscordWebhookDestination, DiscussionOperation, DiscussionReceipt, DiscussionRequest,
    DiscussionResult, MAX_DISCUSSION_CONTENT_CHARS, MAX_DISCUSSION_ID_BYTES, MAX_THREAD_NAME_CHARS,
    discussion_request,
};
pub use transport::DiscordDiscussionTransport;

#[cfg(test)]
#[path = "discord_discussion/characterization.rs"]
mod characterization;
