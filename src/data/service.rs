//! [`DataService`] port implementation. Each method delegates to the inherent `DataBroker`
//! method of the same name.

use super::DataError;
use super::{
    BindingManifest, DataBroker, DataStream, Event, ImpactArtifact, PublicSource,
    RequestedSubscription, SourceConfig, SourceView, TestResult,
};
use crate::{
    ports::{BoxFuture, DataService},
    security::audit::MutationAudit,
};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::mpsc;

impl DataService for DataBroker {
    fn source_views<'a>(
        &'a self,
        org: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<SourceView>, DataError>> {
        Box::pin(DataBroker::source_views(self, org))
    }
    fn source_view_by_id<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<SourceView, DataError>> {
        Box::pin(DataBroker::source_view_by_id(self, id))
    }
    fn create_managed_source(
        &self,
        source: SourceConfig,
        enabled: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<SourceView, DataError>> {
        Box::pin(DataBroker::create_managed_source(
            self, source, enabled, audit,
        ))
    }
    fn set_managed_source<'a>(
        &'a self,
        id: &'a str,
        source: SourceConfig,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<SourceView, DataError>> {
        Box::pin(DataBroker::set_managed_source(
            self, id, source, enabled, expected, audit,
        ))
    }
    fn set_managed_enabled<'a>(
        &'a self,
        id: &'a str,
        enabled: bool,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<SourceView, DataError>> {
        Box::pin(DataBroker::set_managed_enabled(
            self, id, enabled, expected, audit,
        ))
    }
    fn delete_managed<'a>(
        &'a self,
        id: &'a str,
        expected: u64,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), DataError>> {
        Box::pin(DataBroker::delete_managed(self, id, expected, audit))
    }
    fn impact<'a>(
        &'a self,
        source_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ImpactArtifact>, DataError>> {
        Box::pin(DataBroker::impact(self, source_id))
    }
    fn test_source<'a>(
        &'a self,
        id: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<TestResult, DataError>> {
        Box::pin(DataBroker::test_source(self, id, operation, params))
    }
    fn audit_mutation<'a>(
        &'a self,
        audit: MutationAudit,
        operation: &'a str,
        id: &'a str,
        org: &'a str,
        result: &'a str,
        revision: Option<u64>,
    ) -> BoxFuture<'a, Result<(), DataError>> {
        Box::pin(DataBroker::audit_mutation(
            self, audit, operation, id, org, result, revision,
        ))
    }
    fn public_sources<'a>(&'a self, org: Option<&'a str>) -> BoxFuture<'a, Vec<PublicSource>> {
        Box::pin(DataBroker::public_sources(self, org))
    }

    fn try_bindings<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<BindingManifest, DataError>> {
        Box::pin(DataBroker::try_bindings(self, id))
    }
    fn set_bindings<'a>(
        &'a self,
        id: &'a str,
        manifest: BindingManifest,
        org: &'a str,
    ) -> BoxFuture<'a, Result<(), DataError>> {
        Box::pin(DataBroker::set_bindings(self, id, manifest, org))
    }
    fn set_bindings_audited<'a>(
        &'a self,
        id: &'a str,
        manifest: BindingManifest,
        org: &'a str,
        audit: MutationAudit,
    ) -> BoxFuture<'a, Result<(), DataError>> {
        Box::pin(DataBroker::set_bindings_audited(
            self, id, manifest, org, audit,
        ))
    }

    fn query<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, DataError>> {
        Box::pin(DataBroker::query(self, id, binding, operation, params))
    }
    fn query_stream<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        operation: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<mpsc::Receiver<Result<Value, DataError>>, DataError>> {
        Box::pin(DataBroker::query_stream(
            self, id, binding, operation, params,
        ))
    }

    fn set_snapshot<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        key: &'a str,
        value: Value,
    ) -> BoxFuture<'a, Result<u64, DataError>> {
        Box::pin(DataBroker::set_snapshot(self, id, binding, key, value))
    }
    fn append_events<'a>(
        &'a self,
        id: &'a str,
        binding: &'a str,
        subscription: &'a str,
        events: Vec<Event>,
    ) -> BoxFuture<'a, Result<(usize, usize), DataError>> {
        Box::pin(DataBroker::append_events(
            self,
            id,
            binding,
            subscription,
            events,
        ))
    }
    fn stream<'a>(
        &'a self,
        id: &'a str,
        topics: Vec<RequestedSubscription>,
        cursor: BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<DataStream, DataError>> {
        Box::pin(DataBroker::stream(self, id, topics, cursor))
    }
}
