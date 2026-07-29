//! Privacy-safe, low-cardinality MCP telemetry.
//!
//! The recorder intentionally accepts only protocol dimensions, never request arguments,
//! identities, resource URIs, authorization headers, or artifact content. Metrics remain useful
//! to Prometheus while an opaque server-generated request id connects them to the matching
//! structured completion event.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use nanoid::nanoid;

const DURATION_BUCKETS_SECONDS: [f64; 7] = [0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0];

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MetricKey {
    protocol: &'static str,
    operation: &'static str,
    method: &'static str,
    name: &'static str,
    outcome: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct McpMetricLabels {
    pub protocol: &'static str,
    pub operation: &'static str,
    pub method: &'static str,
    pub name: &'static str,
}

impl Default for McpMetricLabels {
    fn default() -> Self {
        Self {
            protocol: "unknown",
            operation: "unknown",
            method: "unknown",
            name: "none",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum McpOutcome {
    Success,
    AuthenticationFailure,
    AuthorizationFailure,
    ValidationFailure,
    OutputValidationFailure,
    ProtocolError,
    ServerFailure,
    Cancelled,
}

impl McpOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::AuthenticationFailure => "authentication_failure",
            Self::AuthorizationFailure => "authorization_failure",
            Self::ValidationFailure => "validation_failure",
            Self::OutputValidationFailure => "output_validation_failure",
            Self::ProtocolError => "protocol_error",
            Self::ServerFailure => "server_failure",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Default)]
struct Series {
    calls: u64,
    duration_sum_seconds: f64,
    duration_buckets: [u64; DURATION_BUCKETS_SECONDS.len() + 1],
    result_bytes: u64,
    result_size_buckets: [u64; 5],
}

#[derive(Clone, Default)]
pub struct McpTelemetry {
    series: Arc<Mutex<BTreeMap<MetricKey, Series>>>,
}

impl McpTelemetry {
    pub fn begin(&self) -> McpObservation {
        McpObservation {
            telemetry: self.clone(),
            request_id: format!("mcp_{}", nanoid!(16)),
            started: Instant::now(),
            labels: McpMetricLabels::default(),
            completed: false,
        }
    }

    fn record(
        &self,
        request_id: &str,
        labels: McpMetricLabels,
        outcome: McpOutcome,
        duration_seconds: f64,
        result_bytes: usize,
    ) {
        let key = MetricKey {
            protocol: labels.protocol,
            operation: labels.operation,
            method: labels.method,
            name: labels.name,
            outcome: outcome.as_str(),
        };
        let mut series = self
            .series
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let item = series.entry(key).or_default();
        item.calls = item.calls.saturating_add(1);
        item.duration_sum_seconds += duration_seconds;
        let duration_bucket = DURATION_BUCKETS_SECONDS
            .iter()
            .position(|bound| duration_seconds <= *bound)
            .unwrap_or(DURATION_BUCKETS_SECONDS.len());
        item.duration_buckets[duration_bucket] =
            item.duration_buckets[duration_bucket].saturating_add(1);
        item.result_bytes = item
            .result_bytes
            .saturating_add(u64::try_from(result_bytes).unwrap_or(u64::MAX));
        let result_bucket = match result_bytes {
            0..=1_024 => 0,
            1_025..=16_384 => 1,
            16_385..=262_144 => 2,
            262_145..=1_048_576 => 3,
            _ => 4,
        };
        item.result_size_buckets[result_bucket] =
            item.result_size_buckets[result_bucket].saturating_add(1);
        drop(series);

        tracing::info!(
            target: "artifact_mcp::mcp",
            request_id,
            protocol = labels.protocol,
            operation = labels.operation,
            method = labels.method,
            name = labels.name,
            outcome = outcome.as_str(),
            duration_ms = duration_seconds * 1_000.0,
            result_size_bucket = result_size_bucket(result_bytes),
            "MCP request completed"
        );
    }

    pub fn render_prometheus(&self) -> String {
        let series = self
            .series
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut output = String::from(
            "# HELP artifact_mcp_requests_total MCP requests by safe protocol dimensions and outcome.\n\
             # TYPE artifact_mcp_requests_total counter\n\
             # HELP artifact_mcp_request_duration_seconds MCP request duration in seconds.\n\
             # TYPE artifact_mcp_request_duration_seconds histogram\n\
             # HELP artifact_mcp_result_bytes_total Total serialized MCP response bytes.\n\
             # TYPE artifact_mcp_result_bytes_total counter\n\
             # HELP artifact_mcp_result_size_bucket_total MCP responses in bounded size bands.\n\
             # TYPE artifact_mcp_result_size_bucket_total counter\n",
        );
        for (key, value) in series.iter() {
            let labels = format!(
                "protocol=\"{}\",operation=\"{}\",method=\"{}\",name=\"{}\",outcome=\"{}\"",
                key.protocol, key.operation, key.method, key.name, key.outcome
            );
            output.push_str(&format!(
                "artifact_mcp_requests_total{{{labels}}} {}\n",
                value.calls
            ));
            let mut cumulative = 0_u64;
            for (index, bound) in DURATION_BUCKETS_SECONDS.iter().enumerate() {
                cumulative = cumulative.saturating_add(value.duration_buckets[index]);
                output.push_str(&format!(
                    "artifact_mcp_request_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {cumulative}\n"
                ));
            }
            cumulative =
                cumulative.saturating_add(value.duration_buckets[DURATION_BUCKETS_SECONDS.len()]);
            output.push_str(&format!(
                "artifact_mcp_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {cumulative}\n\
                 artifact_mcp_request_duration_seconds_sum{{{labels}}} {}\n\
                 artifact_mcp_request_duration_seconds_count{{{labels}}} {}\n\
                 artifact_mcp_result_bytes_total{{{labels}}} {}\n",
                value.duration_sum_seconds, value.calls, value.result_bytes
            ));
            for (bucket, count) in [
                ("le_1k", value.result_size_buckets[0]),
                ("le_16k", value.result_size_buckets[1]),
                ("le_256k", value.result_size_buckets[2]),
                ("le_1m", value.result_size_buckets[3]),
                ("gt_1m", value.result_size_buckets[4]),
            ] {
                output.push_str(&format!(
                    "artifact_mcp_result_size_bucket_total{{{labels},size=\"{bucket}\"}} {count}\n"
                ));
            }
        }
        output
    }
}

pub struct McpObservation {
    telemetry: McpTelemetry,
    request_id: String,
    started: Instant,
    labels: McpMetricLabels,
    completed: bool,
}

impl McpObservation {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn set_labels(&mut self, labels: McpMetricLabels) {
        self.labels = labels;
    }

    pub fn finish(mut self, outcome: McpOutcome, result_bytes: usize) {
        self.completed = true;
        self.telemetry.record(
            &self.request_id,
            self.labels,
            outcome,
            self.started.elapsed().as_secs_f64(),
            result_bytes,
        );
    }
}

impl Drop for McpObservation {
    fn drop(&mut self) {
        if !self.completed {
            self.telemetry.record(
                &self.request_id,
                self.labels,
                McpOutcome::Cancelled,
                self.started.elapsed().as_secs_f64(),
                0,
            );
        }
    }
}

pub fn labels_for(
    protocol: &'static str,
    method: Option<&str>,
    name: Option<&str>,
) -> McpMetricLabels {
    let method = safe_method(method);
    McpMetricLabels {
        protocol,
        operation: operation(method),
        method,
        name: safe_name(method, name),
    }
}

fn operation(method: &str) -> &'static str {
    match method {
        "server/discover" => "discovery",
        "initialize" => "initialization",
        "tools/list" => "listing",
        "tools/call" => "tool_call",
        "resources/list" | "resources/templates/list" | "resources/read" => "resource",
        "tasks/get" | "tasks/update" | "tasks/cancel" => "task",
        _ => "unknown",
    }
}

fn safe_method(method: Option<&str>) -> &'static str {
    match method {
        Some("initialize") => "initialize",
        Some("server/discover") => "server/discover",
        Some("tools/list") => "tools/list",
        Some("tools/call") => "tools/call",
        Some("resources/list") => "resources/list",
        Some("resources/templates/list") => "resources/templates/list",
        Some("resources/read") => "resources/read",
        Some("tasks/get") => "tasks/get",
        Some("tasks/update") => "tasks/update",
        Some("tasks/cancel") => "tasks/cancel",
        _ => "unknown",
    }
}

fn safe_name(method: &str, name: Option<&str>) -> &'static str {
    if method == "resources/read" {
        return match name {
            Some("ui://artifact-mcp/review") => "review_app",
            Some(uri) if uri.starts_with("artifact://") && uri.ends_with("/thumbnail") => {
                "artifact_thumbnail"
            }
            Some(uri) if uri.starts_with("artifact://") => "artifact_content",
            _ => "unknown",
        };
    }
    if method.starts_with("tasks/") {
        return "preview_regeneration";
    }
    if method != "tools/call" {
        return "none";
    }
    match name {
        Some("publish_artifact") => "publish_artifact",
        Some("publish_bundle") => "publish_bundle",
        Some("list_artifacts") => "list_artifacts",
        Some("delete_artifact") => "delete_artifact",
        Some("update_artifact") => "update_artifact",
        Some("set_visibility") => "set_visibility",
        Some("list_categories") => "list_categories",
        Some("set_category") => "set_category",
        Some("create_category") => "create_category",
        Some("delete_category") => "delete_category",
        Some("list_revisions") => "list_revisions",
        Some("create_share") => "create_share",
        Some("list_shares") => "list_shares",
        Some("revoke_share") => "revoke_share",
        Some("artifact_stats") => "artifact_stats",
        Some("submit_feedback") => "submit_feedback",
        Some("list_feedback") => "list_feedback",
        Some("resolve_feedback") => "resolve_feedback",
        Some("reopen_feedback") => "reopen_feedback",
        Some("read_artifact") => "read_artifact",
        Some("patch_artifact") => "patch_artifact",
        Some("regenerate_artifact_preview") => "regenerate_artifact_preview",
        _ => "unknown",
    }
}

fn result_size_bucket(bytes: usize) -> &'static str {
    match bytes {
        0..=1_024 => "le_1k",
        1_025..=16_384 => "le_16k",
        16_385..=262_144 => "le_256k",
        262_145..=1_048_576 => "le_1m",
        _ => "gt_1m",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_keep_only_allowlisted_dimensions_and_bounded_sizes() {
        let telemetry = McpTelemetry::default();
        let mut observation = telemetry.begin();
        observation.set_labels(labels_for(
            "2026-07-28",
            Some("tools/call"),
            Some("secret-from-an-argument"),
        ));
        observation.finish(McpOutcome::Success, 2_048);

        let output = telemetry.render_prometheus();
        assert!(output.contains("method=\"tools/call\",name=\"unknown\",outcome=\"success\""));
        assert!(output.contains("size=\"le_16k\""));
        assert!(!output.contains("secret-from-an-argument"));
    }

    #[test]
    fn dropping_an_unfinished_observation_counts_cancellation() {
        let telemetry = McpTelemetry::default();
        {
            let mut observation = telemetry.begin();
            observation.set_labels(labels_for(
                "2026-07-28",
                Some("tools/call"),
                Some("list_artifacts"),
            ));
        }
        assert!(
            telemetry
                .render_prometheus()
                .contains("name=\"list_artifacts\",outcome=\"cancelled\"")
        );
    }
}
