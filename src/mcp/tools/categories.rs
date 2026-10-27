//! Category tools.

use serde_json::Value;

use crate::{
    AppDeps,
    artifacts::lifecycle::normalize_category,
    error::McpError,
    mcp::{
        args::{category_org, publisher_write, required_string},
        protocol::OrderedJson,
        shape::{object, tool_result},
    },
    model::PublisherIdentity,
    security::audit::MutationAudit,
};

pub(crate) async fn list_categories(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let org = category_org(arguments, auth)?;
    let categories = deps.admin.categories(&org).await?;
    tool_result(object! {
        "org" => OrderedJson::string(org.0),
        "categories" => OrderedJson::Array(categories.into_iter().map(OrderedJson::string).collect()),
    })
}

pub(crate) async fn set_category(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let id = required_string(arguments, "id")?;
    let owned = publisher_write(id, auth, deps).await?;
    let org = owned.meta().org.clone();
    let category = required_string(arguments, "category")?.to_owned();
    let registered_category = normalize_category(&category);
    let audit = MutationAudit::publisher(auth)?;
    // The artifact and organization ports own separate transactions. Make the audited registry
    // write a prerequisite so a failed audit key/write cannot report a successful category
    // mutation while leaving Settings without the category.
    if !registered_category.is_empty() {
        deps.admin
            .add_category(&org, &registered_category, audit.clone())
            .await?;
    }
    let result = deps
        .artifacts
        .set_category(owned.into_authorized(), category, audit)
        .await?;
    tool_result(object! {
        "id" => OrderedJson::string(id),
        "category" => OrderedJson::string(result.category),
    })
}

pub(crate) async fn create_category(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let org = category_org(arguments, auth)?;
    let name = deps
        .admin
        .add_category(
            &org,
            required_string(arguments, "name")?,
            MutationAudit::publisher(auth)?,
        )
        .await?;
    tool_result(object! {
        "org" => OrderedJson::string(org.0),
        "name" => OrderedJson::string(name),
    })
}

pub(crate) async fn delete_category(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<Value, McpError> {
    let org = category_org(arguments, auth)?;
    let name = required_string(arguments, "name")?;
    let removed = deps
        .admin
        .remove_category(&org, name, MutationAudit::publisher(auth)?)
        .await?;
    tool_result(object! {
        "org" => OrderedJson::string(org.0),
        "name" => OrderedJson::string(name),
        "removed" => OrderedJson::Bool(removed),
    })
}
