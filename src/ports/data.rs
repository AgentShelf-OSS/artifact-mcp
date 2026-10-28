//! Live artifact data contract: source administration, bindings, the operation proxy, the push
//! event store, and viewer streams (ADR-0017).
//!
//! Errors are short stable reason strings such as `not_found`, `bad_params`, and
//! `data_unavailable`. Routes map them to HTTP statuses and MCP errors.

use std::collections::BTreeMap;

use serde_json::Value;
use tokio::sync::mpsc;

use super::BoxFuture;
use crate::{
    data::{
        BindingManifest, DataStream, Event, ImpactArtifact, PublicSource, RequestedSubscription,
        SourceConfig, SourceView, TestResult,
    },
    persistence::db::DbPool,
    security::audit::MutationAudit,
};

pub trait DataService: Send + Sync {
    /// Shared database handle, when the service is backed by SQLite. Collection stores reuse it.
    fn pool(&self) -> Option<DbPool>;

    // Source administration.
    fn source_views<'a>(
        &'a self,
        org: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<SourceView>, String>>;
    fn source_view_by_id<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<SourceView, String>>;
    fn create_managed_source(
        &self,
        source: SourceConfig,
        enabled: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<SourceView, String>>;
    fn set_managed_source<'a>(
        &'a self,
        id: &'a str,
        source: SourceConfig,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<SourceView, String>>;
    fn set_managed_enabled<'a>(
        &'a self,
        id: &'a str,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<SourceView, String>>;
    fn delete_managed<'a>(
        &'a self,
        id: &'a str,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), String>>;
    fn impact<'a>(
        &'a self,
        source_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ImpactArtifact>, String>>;
    fn test_source<'a>(
        &'a self,
        id: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<TestResult, String>>;
    fn audit_mutation<'a>(
        &'a self,
        audit: MutationAudit,
        operation: &'a str,
        id: &'a str,
        org: &'a str,
        result: &'a str,
        revision: Option<u64>,
    ) -> BoxFuture<'a, Result<(), String>>;
    fn public_sources<'a>(&'a self, org: Option<&'a str>) -> BoxFuture<'a, Vec<PublicSource>>;

    // Artifact bindings.
    fn try_bindings<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<BindingManifest, String>>;
    fn get_bindings<'a>(&'a self, id: &'a str) -> BoxFuture<'a, BindingManifest>;
    fn set_bindings<'a>(
        &'a self,
        id: &'a str,
        manifest: BindingManifest,
        org: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;
    fn set_bindings_audited<'a>(
        &'a self,
        id: &'a str,
        manifest: BindingManifest,
        org: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), String>>;

    // Operation proxy.
    fn query<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, String>>;
    fn query_stream<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<mpsc::Receiver<Result<Value, String>>, String>>;

    // Push store and viewer streams.
    fn set_snapshot<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        key: &'a str,
        value: Value,
    ) -> BoxFuture<'a, Result<u64, String>>;
    fn append_events<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        subscription: &'a str,
        events: Vec<Event>,
    ) -> BoxFuture<'a, Result<(usize, usize), String>>;
    fn stream<'a>(
        &'a self,
        id: &'a str,
        topics: Vec<RequestedSubscription>,
        cursor: BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DataStream, String>>;
}
