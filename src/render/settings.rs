//! Askama administrator settings page.

use std::collections::BTreeMap;

use askama::Template;

use crate::{
    error::AppError,
    model::{OrgId, WebhookEvent},
    render::{
        portal::{FAVICON, PORTAL_CSS, THEME_BOOT, TrustedStatic, org_color},
        view_models::SettingsView,
    },
};

const SETTINGS_CSS: TrustedStatic = TrustedStatic::new(include_str!("../../assets/settings.css"));
const SETTINGS_SCRIPT: TrustedStatic = TrustedStatic::new(include_str!("../../assets/settings.js"));

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate<'a> {
    favicon: TrustedStatic,
    theme_boot: TrustedStatic,
    portal_css: TrustedStatic,
    settings_css: TrustedStatic,
    script: TrustedStatic,
    site_host: &'a str,
    app_name: &'a str,
    app_brand: &'a str,
    viewer_email: String,
    admin_color: String,
    organization_count: usize,
    active_count: usize,
    revoked_count: usize,
    keys: Vec<KeyTemplate>,
    organizations: Vec<OrganizationTemplate>,
    org_options: Vec<String>,
}

struct KeyTemplate {
    client_id: String,
    label: String,
    has_label: bool,
    org: String,
    org_color: String,
    role: String,
    owner_email: String,
    has_owner: bool,
    search_text: String,
    created_at: String,
    revoked: bool,
}

struct OrganizationTemplate {
    name: String,
    domain_name_collision: bool,
    label: String,
    has_label: bool,
    color: String,
    swatch: String,
    key_count_label: String,
    webhook_count_label: String,
    domains: Vec<String>,
    emails: Vec<String>,
    categories: Vec<String>,
    webhooks: Vec<WebhookTemplate>,
}

struct WebhookTemplate {
    id: String,
    url: String,
    label: String,
    has_label: bool,
    artifact_events: Vec<WebhookEventTemplate>,
    feedback_events: Vec<WebhookEventTemplate>,
}

struct WebhookEventTemplate {
    name: &'static str,
    enabled: bool,
}

pub(super) fn render_settings(
    site_host: &str,
    app_name: &str,
    app_brand: &str,
    view: &SettingsView,
) -> Result<String, AppError> {
    let colors: BTreeMap<OrgId, Option<String>> = view
        .organizations
        .iter()
        .map(|organization| {
            (
                organization.organization.name.clone(),
                organization.organization.color.clone(),
            )
        })
        .collect();
    let active_count = view
        .keys
        .iter()
        .filter(|key| key.revoked_at.is_none())
        .count();
    let keys = view
        .keys
        .iter()
        .map(|key| KeyTemplate {
            client_id: key.client_id.0.clone(),
            label: key.label.clone(),
            has_label: !key.label.is_empty(),
            org: key.org.0.clone(),
            org_color: org_color(&key.org.0, colors.get(&key.org).and_then(Option::as_deref)),
            role: key.role.clone(),
            owner_email: key.owner_email.clone().unwrap_or_default(),
            has_owner: key.owner_email.is_some(),
            search_text: format!(
                "{} {} {} {} {}",
                key.client_id.0,
                key.label,
                key.org.0,
                key.role,
                key.owner_email.as_deref().unwrap_or_default()
            ),
            created_at: format_settings_date(&key.created_at.0),
            revoked: key.revoked_at.is_some(),
        })
        .collect();
    let organizations = view
        .organizations
        .iter()
        .map(|entry| {
            let organization = &entry.organization;
            let color = organization.color.clone().unwrap_or_default();
            OrganizationTemplate {
                name: organization.name.0.clone(),
                domain_name_collision: is_domain_shaped_org(&organization.name.0),
                label: organization.label.clone(),
                has_label: !organization.label.is_empty(),
                color: color.clone(),
                swatch: if valid_picker_color(&color) {
                    color
                } else {
                    "#356b9f".to_owned()
                },
                key_count_label: format!(
                    "{} {}",
                    organization.key_count,
                    if organization.key_count == 1 {
                        "key"
                    } else {
                        "keys"
                    }
                ),
                webhook_count_label: format!(
                    "{} webhook{}",
                    entry.webhooks.len(),
                    if entry.webhooks.len() == 1 { "" } else { "s" }
                ),
                domains: organization.domains.clone(),
                emails: organization.emails.clone(),
                categories: organization.categories.clone(),
                webhooks: entry
                    .webhooks
                    .iter()
                    .map(|webhook| {
                        let mut events = webhook_events(&webhook.events);
                        let feedback_events = events.split_off(4);
                        WebhookTemplate {
                            id: webhook.id.0.clone(),
                            url: webhook.url.clone(),
                            label: webhook.label.clone(),
                            has_label: !webhook.label.is_empty(),
                            artifact_events: events,
                            feedback_events,
                        }
                    })
                    .collect(),
            }
        })
        .collect();
    let organization_count = view.organizations.len();
    let revoked_count = view.keys.len().saturating_sub(active_count);
    let template = SettingsTemplate {
        favicon: FAVICON,
        theme_boot: THEME_BOOT,
        portal_css: PORTAL_CSS,
        settings_css: SETTINGS_CSS,
        script: SETTINGS_SCRIPT,
        site_host,
        app_name,
        app_brand,
        viewer_email: view
            .viewer
            .email
            .as_ref()
            .map_or_else(String::new, |email| email.0.clone()),
        admin_color: org_color("admin", None),
        organization_count,
        active_count,
        revoked_count,
        keys,
        organizations,
        org_options: view
            .organizations
            .iter()
            .map(|entry| entry.organization.name.0.clone())
            .collect(),
    };
    template.render().map_err(|_| AppError::Internal)
}

fn is_domain_shaped_org(value: &str) -> bool {
    let mut labels = value.split('.');
    let first = labels.next().unwrap_or_default();
    let remaining = labels.collect::<Vec<_>>();
    !remaining.is_empty() && is_domain_label(first) && remaining.into_iter().all(is_domain_label)
}

fn is_domain_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    matches!((bytes.first(), bytes.last()), (Some(first), Some(last))
        if first.is_ascii_alphanumeric()
            && last.is_ascii_alphanumeric()
            && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-'))
}

fn webhook_events(enabled: &[WebhookEvent]) -> Vec<WebhookEventTemplate> {
    [
        ("published", WebhookEvent::Published),
        ("updated", WebhookEvent::Updated),
        ("restored", WebhookEvent::Restored),
        ("deleted", WebhookEvent::Deleted),
        ("feedback", WebhookEvent::Feedback),
        ("resolved", WebhookEvent::Resolved),
    ]
    .into_iter()
    .map(|(name, event)| WebhookEventTemplate {
        name,
        enabled: enabled.contains(&event),
    })
    .collect()
}

fn format_settings_date(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        value[..10].to_owned()
    } else {
        String::new()
    }
}

fn valid_picker_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}
