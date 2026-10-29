//! Administrator settings, keys, organizations, and webhooks.

use axum::{
    Router,
    routing::{delete, get, patch, post},
};

use crate::AppDeps;

mod data_sources;
mod keys;
mod organizations;
mod settings;
mod support;

use data_sources::*;
use keys::*;
use organizations::*;
use settings::*;

pub(crate) use support::{admin_json_request, require_admin};
use support::{iso_timestamp, js_or_empty, js_trim, target_audit};

pub(crate) fn router() -> Router<AppDeps> {
    Router::new()
        .route("/settings", get(settings))
        .route("/settings/connections", get(settings))
        .route(
            "/settings/data-sources",
            get(list_data_sources).post(create_data_source),
        )
        .route(
            "/settings/data-sources/{id}",
            get(get_data_source)
                .patch(update_data_source)
                .delete(delete_data_source),
        )
        .route(
            "/settings/data-sources/{id}/enable",
            post(enable_data_source),
        )
        .route(
            "/settings/data-sources/{id}/disable",
            post(disable_data_source),
        )
        .route(
            "/settings/data-sources/{id}/impact",
            get(data_source_impact),
        )
        .route("/settings/data-sources/{id}/test", post(test_data_source))
        .route(
            "/settings/data-bindings/{artifact_id}",
            get(get_data_bindings).put(update_data_bindings),
        )
        .route("/settings/access-sync", get(access_sync_status))
        .route("/settings/keys", post(create_key))
        .route("/settings/keys/{id}", patch(update_key))
        .route("/settings/keys/{id}/revoke", post(revoke_key))
        .route("/settings/keys/{id}/owner", post(set_key_owner))
        .route(
            "/settings/keys/{id}/owner/backfill",
            post(backfill_key_owner),
        )
        .route("/settings/orgs", post(create_org))
        .route("/settings/orgs/{name}", delete(delete_org))
        .route("/settings/orgs/{name}/domains", post(add_domain))
        .route(
            "/settings/orgs/{name}/domains/{domain}",
            delete(remove_domain),
        )
        .route("/settings/orgs/{name}/emails", post(add_email_member))
        .route(
            "/settings/orgs/{name}/emails/{email}",
            delete(remove_email_member),
        )
        .route(
            "/settings/orgs/{name}/categories",
            post(add_category).delete(remove_category),
        )
        .route("/settings/orgs/{name}/color", post(set_color))
        .route("/settings/orgs/{name}/webhooks", post(create_webhook))
        .route(
            "/settings/orgs/{name}/webhooks/{id}",
            delete(remove_webhook).patch(update_webhook_events),
        )
        .route(
            "/settings/orgs/{name}/webhooks/{id}/test",
            post(test_webhook),
        )
}

#[cfg(test)]
mod characterization {
    use serde_json::json;

    use super::{js_or_empty, object_keys_allowed, parse_webhook_events};

    #[test]
    fn javascript_coercion_preserves_nested_array_and_number_forms() {
        assert_eq!(
            js_or_empty(Some(&json!(["alpha", [null, true, 0.000001], {"x": 1}]))),
            "alpha,,true,0.000001,[object Object]"
        );
        assert_eq!(js_or_empty(Some(&json!(1e21))), "1e+21");
        assert_eq!(js_or_empty(Some(&json!(0))), "");
    }

    #[test]
    fn webhook_events_reject_unknown_values_after_known_prefix() {
        let error = parse_webhook_events(Some(&json!(["published", "unknown"])))
            .expect_err("unknown event is rejected");

        assert_eq!(error.to_string(), "Unknown webhook event: unknown");
    }

    #[test]
    fn object_key_filter_requires_an_object_and_rejects_extra_keys() {
        assert!(object_keys_allowed(&json!({"enabled": true}), &["enabled"]));
        assert!(!object_keys_allowed(
            &json!({"enabled": true, "extra": false}),
            &["enabled"]
        ));
        assert!(!object_keys_allowed(&json!(["enabled"]), &["enabled"]));
    }
}
