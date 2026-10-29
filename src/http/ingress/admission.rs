//! Origin-side request admission controls.
//!
//! The facade preserves the existing ingress API while cohesive implementation modules keep
//! request admission, stateful limits, and streamed body handling independently navigable.

mod body;
mod middleware;
mod state;

pub use body::{
    BodyReadError, body_error_response, complexity_response, discard_body_bounded,
    read_body_limited, validate_json_complexity,
};
pub use middleware::{admit, mcp_admission_response, peer_addr};
pub use state::{
    IngressMetrics, IngressState, McpCost, McpRequestPermit, RenderQueuePressure, ViewerCost,
};

#[cfg(test)]
mod tests;
