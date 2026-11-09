//! Live artifact data contract: source administration, bindings, the operation proxy, the push
//! event store, and viewer streams (ADR-0017).
//!
//! Typed errors retain stable reason tokens such as `not_found`, `bad_params`, and
//! `data_unavailable`. Routes map variants to HTTP statuses and MCP errors.

use std::collections::BTreeMap;

use serde_json::Value;
use tokio::sync::mpsc;

use super::BoxFuture;
pub use crate::data::DataError;
use crate::{
    data::{
        BindingManifest, DataStream, Event, ImpactArtifact, PublicSource, RequestedSubscription,
        SourceConfig, SourceView, TestResult,
    },
    security::audit::MutationAudit,
};

pub trait DataService: Send + Sync {
    // Source administration.
    fn source_views<'a>(
        &'a self,
        org: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<SourceView>, DataError>>;
    fn source_view_by_id<'a>(&'a self, id: &'a str)
    -> BoxFuture<'a, Result<SourceView, DataError>>;
    fn create_managed_source(
        &self,
        source: SourceConfig,
        enabled: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<SourceView, DataError>>;
    fn set_managed_source<'a>(
        &'a self,
        id: &'a str,
        source: SourceConfig,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<SourceView, DataError>>;
    fn set_managed_enabled<'a>(
        &'a self,
        id: &'a str,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<SourceView, DataError>>;
    fn delete_managed<'a>(
        &'a self,
        id: &'a str,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), DataError>>;
    fn impact<'a>(
        &'a self,
        source_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ImpactArtifact>, DataError>>;
    fn test_source<'a>(
        &'a self,
        id: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<TestResult, DataError>>;
    fn audit_mutation<'a>(
        &'a self,
        audit: MutationAudit,
        operation: &'a str,
        id: &'a str,
        org: &'a str,
        result: &'a str,
        revision: Option<u64>,
    ) -> BoxFuture<'a, Result<(), DataError>>;
    fn public_sources<'a>(&'a self, org: Option<&'a str>) -> BoxFuture<'a, Vec<PublicSource>>;

    // Artifact bindings.
    fn try_bindings<'a>(&'a self, id: &'a str)
    -> BoxFuture<'a, Result<BindingManifest, DataError>>;
    fn set_bindings<'a>(
        &'a self,
        id: &'a str,
        manifest: BindingManifest,
        org: &'a str,
    ) -> BoxFuture<'a, Result<(), DataError>>;
    fn set_bindings_audited<'a>(
        &'a self,
        id: &'a str,
        manifest: BindingManifest,
        org: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), DataError>>;

    // Operation proxy.
    fn query<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, DataError>>;
    fn query_stream<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<mpsc::Receiver<Result<Value, DataError>>, DataError>>;

    // Push store and viewer streams.
    fn set_snapshot<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        key: &'a str,
        value: Value,
    ) -> BoxFuture<'a, Result<u64, DataError>>;
    fn append_events<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        subscription: &'a str,
        events: Vec<Event>,
    ) -> BoxFuture<'a, Result<(usize, usize), DataError>>;
    fn stream<'a>(
        &'a self,
        id: &'a str,
        topics: Vec<RequestedSubscription>,
        cursor: BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DataStream, DataError>>;
}
