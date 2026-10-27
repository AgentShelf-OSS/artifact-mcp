//! Tool argument parsing and publisher authorization helpers.

use crate::{
    AppDeps,
    artifacts::validation::js_trim,
    error::{AppError, JsonRpcError, McpError},
    model::{ArtifactId, OrgId, PublisherIdentity},
    security::access::{AccessPolicy, OwnedArtifact, PUBLISH_PERMISSION_ERROR},
};

use super::protocol::OrderedJson;

pub(crate) async fn publisher_read(
    id: &str,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OwnedArtifact, McpError> {
    let meta = deps.artifacts.find_meta(&ArtifactId::from(id)).await?;
    Ok(AccessPolicy::authorize_publisher_read(auth, meta, id)?)
}

pub(crate) async fn publisher_write(
    id: &str,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OwnedArtifact, McpError> {
    let meta = deps.artifacts.find_meta(&ArtifactId::from(id)).await?;
    Ok(AccessPolicy::authorize_publisher_write(auth, meta, id, "")?)
}

pub(crate) async fn publisher_delete(
    id: &str,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OwnedArtifact, McpError> {
    let meta = deps.artifacts.find_meta(&ArtifactId::from(id)).await?;
    Ok(AccessPolicy::authorize_publisher_delete(auth, meta, id)?)
}

pub(crate) fn ensure_can_publish(auth: &PublisherIdentity) -> Result<(), McpError> {
    if !auth.is_admin() && auth.role == "reader" {
        Err(AppError::Forbidden(PUBLISH_PERMISSION_ERROR.to_owned()).into())
    } else {
        Ok(())
    }
}

pub(crate) fn required_string<'a>(
    arguments: &'a OrderedJson,
    key: &str,
) -> Result<&'a str, McpError> {
    arguments
        .get(key)
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| JsonRpcError::InvalidParams(format!("{key} is required")).into())
}

pub(crate) fn nonempty_protocol_string<'a>(
    arguments: &'a OrderedJson,
    key: &str,
) -> Result<&'a str, McpError> {
    let value = required_string(arguments, key)?;
    if value.is_empty() {
        Err(JsonRpcError::InvalidParams(format!("{key} is required")).into())
    } else {
        Ok(value)
    }
}

pub(crate) fn optional_string(arguments: &OrderedJson, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(OrderedJson::as_str)
        .map(ToOwned::to_owned)
}

pub(crate) fn bundle_files(value: Option<&OrderedJson>) -> Result<Vec<(String, String)>, McpError> {
    let Some(value) = value else {
        return Err(JsonRpcError::InvalidParams("files is required".to_owned()).into());
    };
    let mut files = Vec::new();
    for (path, content) in value.object_entries() {
        let Some(content) = content.as_str() else {
            return Err(
                JsonRpcError::InvalidParams(format!("files.{path} must be a string")).into(),
            );
        };
        files.push((path.to_owned(), content.to_owned()));
    }
    Ok(files)
}

pub(crate) async fn target_org(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Result<OrgId, McpError> {
    if AccessPolicy::publisher_is_admin(auth)
        && let Some(org) = arguments.get("org").and_then(OrderedJson::as_str)
    {
        let normalized = js_trim(org).to_lowercase();
        if !normalized.is_empty() {
            let target = OrgId::from(normalized.clone());
            if !is_valid_publish_org(&normalized) || !deps.admin.org_exists(&target).await? {
                return Err(AppError::Validation(format!(
                    "Unknown organization \"{normalized}\". Create it in the Organizations section first."
                ))
                .into());
            }
            return Ok(target);
        }
    }
    Ok(auth.org.clone())
}

fn is_valid_publish_org(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.clone().count() < 41
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

pub(crate) fn category_org(
    arguments: &OrderedJson,
    auth: &PublisherIdentity,
) -> Result<OrgId, McpError> {
    if !AccessPolicy::publisher_is_admin(auth) {
        return Ok(auth.org.clone());
    }
    let org = arguments
        .get("org")
        .and_then(OrderedJson::as_str)
        .map(js_trim)
        .unwrap_or_default();
    if org.is_empty() {
        Err(AppError::Validation("org is required for admin keys".to_owned()).into())
    } else {
        Ok(OrgId::from(org))
    }
}

pub(crate) fn positive_integer(value: &OrderedJson, name: &str) -> Result<u64, McpError> {
    let valid = value
        .as_number()
        .and_then(serde_json::Number::as_f64)
        .filter(|number| number.is_finite() && *number >= 1.0 && number.fract() == 0.0);
    let Some(number) = valid else {
        return Err(
            JsonRpcError::InvalidParams(format!("{name} must be a positive integer")).into(),
        );
    };
    Ok(if number >= u64::MAX as f64 {
        u64::MAX
    } else {
        number as u64
    })
}

pub(crate) fn non_negative_integer(value: &OrderedJson, name: &str) -> Result<u64, McpError> {
    let valid = value
        .as_number()
        .and_then(serde_json::Number::as_f64)
        .filter(|number| {
            number.is_finite()
                && *number >= 0.0
                && *number <= 9_007_199_254_740_991.0
                && number.fract() == 0.0
        });
    let Some(number) = valid else {
        return Err(
            JsonRpcError::InvalidParams(format!("{name} must be a non-negative integer")).into(),
        );
    };
    Ok(if number >= u64::MAX as f64 {
        u64::MAX
    } else {
        number as u64
    })
}

pub(crate) fn safe_positive_integer(value: &OrderedJson, name: &str) -> Result<u64, McpError> {
    let valid = value
        .as_number()
        .and_then(serde_json::Number::as_f64)
        .filter(|number| {
            number.is_finite()
                && *number >= 1.0
                && *number <= 9_007_199_254_740_991.0
                && number.fract() == 0.0
        });
    let Some(number) = valid else {
        return Err(
            JsonRpcError::InvalidParams(format!("{name} must be a positive integer")).into(),
        );
    };
    Ok(number as u64)
}
