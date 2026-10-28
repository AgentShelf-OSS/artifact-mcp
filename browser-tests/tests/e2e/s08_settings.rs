// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `settings administration`, from `playwright/tests/08-settings.spec.mjs`.

use browser_tests::e2e::{CREDENTIAL_VARIABLE, Case, Ctx, Identity, encode_component};
use browser_tests::{Error, Role, e2e_case, expect, regex};
use serde_json::{Value, json};

const SPEC: &str = "08-settings";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(SPEC, "settings page renders for an admin", settings_renders),
        e2e_case!(
            SPEC,
            "administration keeps compact usable controls in dark phone layout",
            dark_phone_layout
        ),
        e2e_case!(
            SPEC,
            "org lifecycle: create, add domain and email, set colour, delete",
            org_lifecycle
        ),
        e2e_case!(
            SPEC,
            "publisher key: create shows the secret once, then revoke",
            publisher_key_once
        ),
        e2e_case!(
            SPEC,
            "existing publisher keys can fill missing metadata without changing identity",
            key_metadata_edit
        ),
        e2e_case!(
            SPEC,
            "publisher owner assignment previews a null-only legacy backfill",
            owner_backfill_preview
        ),
        e2e_case!(SPEC, "webhook add and delete", webhook_add_delete),
        e2e_case!(
            SPEC,
            "Discord threading settings keep the organization token write-only and explain inheritance",
            discord_threading_settings
        ),
        e2e_case!(
            SPEC,
            "a non-admin viewer cannot reach settings",
            non_admin_settings
        ),
        e2e_case!(
            SPEC,
            "connection credentials are chosen from the server's managed variables",
            connection_credential_picker
        ),
    ]
}

async fn settings_renders(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    page.goto("/settings").await?;
    expect(&page.get_by_role(Role::new("heading").name(regex("Operate the registry", "i"))))
        .to_be_visible()
        .await?;
    expect(&page.get_by_role(Role::new("tab").name(regex("Organizations", "i"))))
        .to_be_visible()
        .await
}

async fn dark_phone_layout(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    page.set_viewport(390, 844, 1.0).await?;
    page.add_init_script(r#"(() => localStorage.setItem("artifact-theme", "dark"))()"#)
        .await?;
    page.goto("/settings").await?;

    expect(&page.locator("html"))
        .to_have_attribute("data-theme", "dark")
        .await?;
    expect(&page.get_by_role(Role::new("heading").name(regex("Operate the registry", "i"))))
        .to_be_visible()
        .await?;
    let dimensions = page
        .evaluate(
            r#"(() => {
      const root = document.documentElement;
      const clientWidth = root.clientWidth;
      return {
        clientWidth,
        scrollWidth: root.scrollWidth,
        offenders: Array.from(document.querySelectorAll("*"), (node) => {
          const rect = node.getBoundingClientRect();
          const style = getComputedStyle(node);
          return {
            selector: `${node.tagName.toLowerCase()}${node.id ? `#${node.id}` : ""}${typeof node.className === "string" && node.className ? `.${node.className.trim().replace(/\s+/g, ".")}` : ""}`,
            left: Math.round(rect.left),
            right: Math.round(rect.right),
            width: Math.round(rect.width),
            clientWidth: node.clientWidth,
            scrollWidth: node.scrollWidth,
            overflowX: style.overflowX,
            minWidth: style.minWidth,
          };
        })
          .filter((entry) => entry.left < -1 || entry.right > clientWidth + 1 || (entry.overflowX === "visible" && entry.scrollWidth > entry.clientWidth + 1))
          .sort((a, b) => Math.max(b.right, b.scrollWidth) - Math.max(a.right, a.scrollWidth))
          .slice(0, 12),
        targetHeights: Array.from(document.querySelectorAll(".admin-tab, .new-org-trigger, .primary-button, .secondary-button, .field input, .field select"), (node) => node.getBoundingClientRect())
          .filter((rect) => rect.width > 0 && rect.height > 0)
          .map((rect) => rect.height),
      };
    })()"#,
        )
        .await?;
    let client_width = dimensions["clientWidth"].as_f64().unwrap_or(0.0);
    let scroll_width = dimensions["scrollWidth"].as_f64().unwrap_or(f64::MAX);
    assert!(
        scroll_width <= client_width + 1.0,
        "{}",
        dimensions["offenders"]
    );
    let min_height = dimensions["targetHeights"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .fold(f64::INFINITY, f64::min);
    assert!(min_height >= 44.0, "{}", dimensions["targetHeights"]);
    Ok(())
}

async fn org_lifecycle(t: Ctx) -> Result<(), Error> {
    let name = format!("pwtmp-{}", t.run_id);
    let created = t
        .api(
            "POST",
            "/settings/orgs",
            Some(json!({"name": name, "label": "Temp"})),
        )
        .await?;
    assert_eq!(created.status, 200, "{}", created.text());

    let domain = t
        .api(
            "POST",
            &format!("/settings/orgs/{name}/domains"),
            Some(json!({"domain": format!("{name}.example")})),
        )
        .await?;
    assert_eq!(domain.status, 200);
    let undomain = t
        .api(
            "DELETE",
            &format!("/settings/orgs/{name}/domains/{name}.example"),
            None,
        )
        .await?;
    assert!([200, 204].contains(&undomain.status), "{}", undomain.status);

    let email = format!("a@{name}.example");
    let mail = t
        .api(
            "POST",
            &format!("/settings/orgs/{name}/emails"),
            Some(json!({"email": email})),
        )
        .await?;
    assert_eq!(mail.status, 200);
    let unmail = t
        .api(
            "DELETE",
            &format!("/settings/orgs/{name}/emails/{}", encode_component(&email)),
            None,
        )
        .await?;
    assert!([200, 204].contains(&unmail.status), "{}", unmail.status);

    let colour = t
        .api(
            "POST",
            &format!("/settings/orgs/{name}/color"),
            Some(json!({"color": "#356B9F"})),
        )
        .await?;
    assert_eq!(colour.status, 200);

    let removed = t
        .api("DELETE", &format!("/settings/orgs/{name}"), None)
        .await?;
    assert!([200, 204].contains(&removed.status), "{}", removed.status);
    Ok(())
}

async fn publisher_key_once(t: Ctx) -> Result<(), Error> {
    let client_id = format!("pwk-{}-2", t.run_id);
    let created = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({"clientId": client_id, "org": t.org, "label": "pw"})),
        )
        .await?;
    assert_eq!(created.status, 200, "{}", created.text());
    let body = created.json();
    let secret = body["secret"].as_str().or_else(|| body["key"].as_str());
    assert!(
        secret.is_some_and(|secret| !secret.is_empty()),
        "secret must be returned once at creation"
    );

    let revoked = t
        .api(
            "POST",
            &format!("/settings/keys/{}/revoke", encode_component(&client_id)),
            Some(json!({})),
        )
        .await?;
    assert!([200, 204].contains(&revoked.status), "{}", revoked.status);
    Ok(())
}

async fn key_metadata_edit(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let org = &t.org;
    let client_id = format!("pwedit-{}", t.run_id);
    let owner_email = format!("editor-{}@example.test", t.run_id);
    let member = t
        .api(
            "POST",
            &format!("/settings/orgs/{}/emails", encode_component(org)),
            Some(json!({"email": owner_email})),
        )
        .await?;
    assert_eq!(member.status, 200, "{}", member.text());
    let created = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({"clientId": client_id, "org": org, "label": "", "role": "author", "ownerEmail": ""})),
        )
        .await?;
    assert_eq!(created.status, 200, "{}", created.text());

    page.goto("/settings#tab=keys").await?;
    let row = page.locator(&format!(r#"#keys tr[data-id="{client_id}"]"#));
    expect(&row).to_be_visible().await?;
    expect(&row.locator(".key-label"))
        .to_have_text("Not labeled")
        .await?;
    row.get_by_role(Role::new("button").name("Edit"))
        .click()
        .await?;

    let editor = page.locator(".key-edit-row");
    expect(&editor).to_be_visible().await?;
    expect(&editor.get_by_text(regex(format!("organization {org}"), "")))
        .to_be_visible()
        .await?;
    expect(&editor.locator(r#"input[name="label"]"#))
        .to_have_value("")
        .await?;
    editor
        .locator(r#"input[name="label"]"#)
        .fill("Alex publisher")
        .await?;
    editor
        .locator(r#"select[name="role"]"#)
        .select_option(&["collaborator"])
        .await?;
    editor
        .locator(r#"input[name="ownerEmail"]"#)
        .fill(&owner_email)
        .await?;

    let key_path = format!("/settings/keys/{client_id}");
    let update_request = page.wait_for_request(move |candidate| {
        candidate.method == "PATCH" && candidate.url.ends_with(&key_path)
    });
    editor
        .get_by_role(Role::new("button").name("Save changes"))
        .click()
        .await?;
    let sent = update_request.await?;
    assert_eq!(
        sent.post_data_json(),
        Some(json!({"label": "Alex publisher", "role": "collaborator", "ownerEmail": owner_email}))
    );
    expect(&editor.locator(".key-edit-status"))
        .to_contain_text("Saved")
        .await?;
    expect(&row.locator(".key-label"))
        .to_have_text("Alex publisher")
        .await?;
    expect(&row.locator(".key-role"))
        .to_have_text("collaborator")
        .await?;
    let owner_card = page.locator(&format!(r#"[data-key-owner-id="{client_id}"]"#));
    expect(&owner_card.locator(r#"input[type="email"]"#))
        .to_have_value(owner_email.as_str())
        .await?;

    page.reload().await?;
    page.get_by_role(Role::new("tab").name(regex("Publisher keys", "")))
        .click()
        .await?;
    let persisted = page.locator(&format!(r#"#keys tr[data-id="{client_id}"]"#));
    expect(&persisted.locator(".key-label"))
        .to_have_text("Alex publisher")
        .await?;
    expect(&persisted.locator(".key-role"))
        .to_have_text("collaborator")
        .await?;
    persisted
        .get_by_role(Role::new("button").name("Edit"))
        .click()
        .await?;
    expect(&page.locator(r#".key-edit-row input[name="ownerEmail"]"#))
        .to_have_value(owner_email.as_str())
        .await
}

async fn owner_backfill_preview(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let org = &t.org;
    let client_id = format!("pwowner-{}", t.run_id);
    let owner_email = format!("owner-{}@example.test", t.run_id);
    let member = t
        .api(
            "POST",
            &format!("/settings/orgs/{}/emails", encode_component(org)),
            Some(json!({"email": owner_email})),
        )
        .await?;
    assert_eq!(member.status, 200, "{}", member.text());
    page.goto("/settings").await?;
    page.get_by_role(Role::new("tab").name(regex("Publisher keys", "")))
        .click()
        .await?;
    page.locator("#name").fill(&client_id).await?;
    page.locator("#label").fill("owner-ui").await?;
    page.locator("#org").select_option(&[org]).await?;
    page.locator("#owner-email").fill(&owner_email).await?;
    page.get_by_role(Role::new("button").name("Generate key"))
        .click()
        .await?;
    expect(&page.locator("#reveal"))
        .to_have_class(regex("show", ""))
        .await?;
    let owner_card = page.locator(&format!(r#"[data-key-owner-id="{client_id}"]"#));
    expect(&owner_card).to_be_visible().await?;
    expect(&owner_card.locator(".owner-current"))
        .to_have_text(format!("Human-owned author key: {owner_email}"))
        .await?;
    owner_card
        .locator(r#"input[type="email"]"#)
        .fill(&owner_email)
        .await?;
    owner_card
        .get_by_role(Role::new("button").name("Save binding"))
        .click()
        .await?;
    expect(&owner_card.locator(".owner-current"))
        .to_have_text(format!("Human-owned author key: {owner_email}"))
        .await?;
    expect(&owner_card.locator(".inline-status"))
        .to_contain_text("future publishes only")
        .await?;

    owner_card
        .get_by_role(Role::new("button").name("Preview legacy backfill"))
        .click()
        .await?;
    expect(&owner_card.locator(".backfill-count"))
        .to_contain_text("0 null-owner artifacts")
        .await?;
    expect(&owner_card.get_by_role(Role::new("button").name("Confirm backfill")))
        .to_be_visible()
        .await
}

async fn webhook_add_delete(t: Ctx) -> Result<(), Error> {
    let path = format!("/settings/orgs/{}/webhooks", encode_component(&t.org));
    let added = t
        .api(
            "POST",
            &path,
            Some(json!({"url": "https://discord.com/api/webhooks/1/pwtest-token", "label": "pw"})),
        )
        .await?;
    assert_eq!(added.status, 200, "{}", added.text());
    let hook = added.json();
    let id = [&hook["id"], &hook["webhook"]["id"]]
        .into_iter()
        .find(|id| !id.is_null() && **id != json!("") && **id != json!(0))
        .map(|id| match id {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
    if let Some(id) = id {
        let deleted = t.api("DELETE", &format!("{path}/{id}"), None).await?;
        assert!([200, 204].contains(&deleted.status), "{}", deleted.status);
    }
    Ok(())
}

async fn discord_threading_settings(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let discussion_org = format!("{}-discussion", t.org);
    let created = t
        .api(
            "POST",
            "/settings/orgs",
            Some(json!({"name": discussion_org, "label": "Discussion Test"})),
        )
        .await?;
    assert_eq!(created.status, 200, "{}", created.text());
    let disabled = t
        .api(
            "PUT",
            &format!(
                "/settings/orgs/{}/discord-threading",
                encode_component(&discussion_org)
            ),
            Some(json!({"botToken": "", "enabled": false})),
        )
        .await?;
    assert_eq!(disabled.status, 200, "{}", disabled.text());
    let disabled_body = disabled.json();
    assert_eq!(disabled_body["enabled"], json!(false));
    // `not.toMatch(/botToken|ciphertext|nonce|tag/i)`.
    let serialized = disabled_body.to_string().to_lowercase();
    for word in ["bottoken", "ciphertext", "nonce", "tag"] {
        assert!(!serialized.contains(word), "{disabled_body}");
    }

    page.goto(&format!(
        "/settings#tab=notifications&org={}",
        encode_component(&discussion_org)
    ))
    .await?;
    let card = page.locator(&format!(r#"[data-discussion-org="{discussion_org}"]"#));
    let token = card.locator(r#"input[name="botToken"]"#);
    expect(&card.get_by_role(Role::new("heading").name("Discord notification threads")))
        .to_be_visible()
        .await?;
    expect(&token).to_have_attribute("type", "password").await?;
    expect(&token).to_have_value("").await?;
    expect(&card.get_by_role(
        Role::new("checkbox").name(regex("Enable Discord threads for this organization", "i")),
    ))
    .to_be_visible()
    .await?;
    expect(&card.get_by_text(regex(
        "Eligible artifacts use this organization default",
        "i",
    )))
    .to_be_visible()
    .await?;
    expect(&card.get_by_text(regex("exact canonical artifact URL", "i")))
        .to_be_visible()
        .await?;

    let removed = t
        .api(
            "DELETE",
            &format!("/settings/orgs/{}", encode_component(&discussion_org)),
            None,
        )
        .await?;
    assert_eq!(removed.status, 200, "{}", removed.text());
    Ok(())
}

async fn non_admin_settings(t: Ctx) -> Result<(), Error> {
    let response = t
        .request_as(&Identity::email("nobody@outsider.example"))
        .get(&format!("{}/settings", t.base_url()))
        .await?;
    assert_eq!(response.status, 403);
    Ok(())
}

/// The values and labels of the credential dropdown in each credential row, in order.
const CREDENTIAL_OPTIONS: &str = r#"(() => [...document.querySelectorAll('#conn-headers select[data-field="env"]')]
  .map(select => [...select.options].map(option => [option.value, option.textContent])))()"#;

async fn connection_credential_picker(t: Ctx) -> Result<(), Error> {
    let id = format!("pwcred-{}", t.run_id);
    let unset = format!("{CREDENTIAL_VARIABLE}_UNSET");
    let created = t
        .api(
            "POST",
            "/settings/data-sources",
            Some(json!({"enabled": false, "definition": {
                "id": id, "org": t.org, "kind": "http", "base_url": "http://127.0.0.1:9",
                "headers_env": {"Authorization": unset},
                "operations": {"status": {"path": "/status"}}
            }})),
        )
        .await?;
    assert_eq!(created.status, 201, "{}", created.text());
    let version = created.json()["version"].clone();

    let page = t.page().await?;
    page.goto("/settings").await?;
    page.locator(r#"[data-admin-tab="connections"]"#)
        .click()
        .await?;
    page.locator(&format!(r#"[data-connection-id="{id}"]"#))
        .click()
        .await?;

    // A saved name the server lacks stays selected and is marked.
    let saved = page.locator(r#"#conn-headers select[data-field="env"]"#);
    expect(&saved).to_have_value(unset.as_str()).await?;
    let options = page.evaluate(CREDENTIAL_OPTIONS).await?;
    assert_eq!(
        options,
        json!([[
            ["", "Choose a variable"],
            [CREDENTIAL_VARIABLE, CREDENTIAL_VARIABLE],
            [unset, format!("{unset} (unavailable)")]
        ]]),
        "saved connection"
    );

    // A new reference offers only the managed variables, never other process variables.
    page.get_by_role(Role::new("button").name("New connection"))
        .click()
        .await?;
    page.get_by_role(Role::new("button").name("Add reference"))
        .click()
        .await?;
    let options = page.evaluate(CREDENTIAL_OPTIONS).await?;
    assert_eq!(
        options,
        json!([[
            ["", "Choose a variable"],
            [CREDENTIAL_VARIABLE, CREDENTIAL_VARIABLE]
        ]]),
        "new connection"
    );
    let picked = page.locator(r#"#conn-headers select[data-field="env"]"#);
    picked.select_option(&[CREDENTIAL_VARIABLE]).await?;
    expect(&picked).to_have_value(CREDENTIAL_VARIABLE).await?;

    let deleted = t
        .api(
            "DELETE",
            &format!("/settings/data-sources/{}", encode_component(&id)),
            Some(json!({"expected_version": version})),
        )
        .await?;
    assert!([200, 204].contains(&deleted.status), "{}", deleted.text());
    Ok(())
}
