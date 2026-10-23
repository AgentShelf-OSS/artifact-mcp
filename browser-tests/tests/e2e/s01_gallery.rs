// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `gallery`, from `playwright/tests/01-gallery.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, Identity, now_millis};
use browser_tests::{Error, Role, e2e_case, exact, expect};
use serde_json::{Value, json};

const SPEC: &str = "01-gallery";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "renders and lists an artifact published into the throwaway org",
            renders_and_lists
        ),
        e2e_case!(
            SPEC,
            "signed-out visitors get the sign-in page, not the gallery",
            signed_out_visitors
        ),
        e2e_case!(
            SPEC,
            "notifications seen endpoint accepts a post",
            notifications_seen
        ),
        e2e_case!(
            SPEC,
            "phone-width dark gallery has no horizontal document overflow",
            phone_dark_gallery
        ),
        e2e_case!(
            SPEC,
            "library keeps every filter in its top toolbar without a rail",
            toolbar_filters
        ),
        e2e_case!(
            SPEC,
            "reset clears gallery filters, search, and sorting without a page error",
            reset_clears_filters
        ),
        e2e_case!(
            SPEC,
            "tablet toolbar keeps organization and category filters reachable",
            tablet_toolbar
        ),
        e2e_case!(
            SPEC,
            "organization changes scope both category pickers and allow inline category creation",
            organization_scopes_categories
        ),
        e2e_case!(
            SPEC,
            "viewer Back link restores the library scroll snapshot",
            back_link_restores_scroll
        ),
    ]
}

fn secret(body: &Value) -> String {
    body["secret"]
        .as_str()
        .or_else(|| body["key"].as_str())
        .unwrap_or("")
        .to_owned()
}

async fn renders_and_lists(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let title = format!("PW Gallery {}", t.org);
    let artifact = t
        .publish(&key, &title, "<!doctype html><h1>g</h1>", None)
        .await?;
    assert!(
        artifact["id"].as_str().is_some_and(|id| !id.is_empty()),
        "publish should return an id"
    );
    page.goto("/").await?;
    expect(&page.get_by_text(&title)).to_be_visible().await
}

async fn signed_out_visitors(t: Ctx) -> Result<(), Error> {
    // A context with `extraHTTPHeaders: {}` sends no identity.
    let response = t.request_as(&Identity::Anonymous).get("/").await?;
    assert_eq!(response.status, 403);
    Ok(())
}

async fn notifications_seen(t: Ctx) -> Result<(), Error> {
    let response = t
        .api("POST", "/notifications/seen", Some(json!({})))
        .await?;
    assert!([200, 204].contains(&response.status), "{}", response.status);
    Ok(())
}

fn min_height(dimensions: &Value) -> f64 {
    dimensions["targetHeights"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .fold(f64::INFINITY, f64::min)
}

async fn phone_dark_gallery(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    page.set_viewport(390, 844, 1.0).await?;
    page.add_init_script(r#"(() => localStorage.setItem("artifact-theme", "dark"))()"#)
        .await?;
    page.goto("/").await?;

    expect(&page.locator("html"))
        .to_have_attribute("data-theme", "dark")
        .await?;
    expect(&page.get_by_role("searchbox"))
        .to_be_visible()
        .await?;
    let dimensions = page
        .evaluate(
            r#"({
              clientWidth: document.documentElement.clientWidth,
              scrollWidth: document.documentElement.scrollWidth,
              targetHeights: Array.from(document.querySelectorAll(".filter-choice, .reset-filters, .layout-toggle button, .select-control select"), (node) => node.getBoundingClientRect())
                .filter((rect) => rect.width > 0 && rect.height > 0)
                .map((rect) => rect.height),
            })"#,
        )
        .await?;
    let client_width = dimensions["clientWidth"].as_f64().unwrap_or(0.0);
    let scroll_width = dimensions["scrollWidth"].as_f64().unwrap_or(f64::MAX);
    assert!(scroll_width <= client_width + 1.0, "{dimensions}");
    assert!(min_height(&dimensions) >= 44.0, "{dimensions}");
    Ok(())
}

async fn toolbar_filters(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    page.goto("/").await?;

    let toolbar = page.locator(".collection-tools");
    expect(&toolbar.get_by_role(Role::new("searchbox").name("Search artifacts")))
        .to_be_visible()
        .await?;
    expect(&toolbar.get_by_label("Show artifacts"))
        .to_be_visible()
        .await?;
    expect(&toolbar.get_by_label("Filter by organization"))
        .to_be_visible()
        .await?;
    expect(&toolbar.get_by_label("Filter by category"))
        .to_be_visible()
        .await?;
    expect(
        &page
            .locator("#library-controls")
            .get_by_label("Sort artifacts"),
    )
    .to_be_visible()
    .await?;
    page.locator(r#"[data-library-view="all"]"#).click().await?;
    expect(
        &page
            .locator("#library-controls")
            .get_by_label(exact("Artifact layout")),
    )
    .to_be_visible()
    .await?;
    expect(&toolbar.locator("[data-reset-filters]"))
        .to_be_hidden()
        .await?;
    expect(&page.locator(".filter-rail")).to_have_count(0).await
}

async fn reset_clears_filters(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let title = format!("PW Reset {}", t.org);
    let other_org = format!("{}-reset-other", t.org);
    let created = t
        .api(
            "POST",
            "/settings/orgs",
            Some(json!({"name": other_org, "label": "Playwright reset organization"})),
        )
        .await?;
    assert!(
        [200, 201, 400].contains(&created.status),
        "{}",
        created.status
    );
    let key_response = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({"clientId": format!("pw-reset-{}", now_millis()), "org": other_org, "label": "playwright reset"})),
        )
        .await?;
    let other_key = secret(&key_response.json());
    t.publish(
        &other_key,
        &format!("PW Reset {other_org}"),
        "<!doctype html><h1>other</h1>",
        Some("Specs"),
    )
    .await?;
    t.publish(
        &key,
        &title,
        "<!doctype html><h1>reset</h1>",
        Some("Reports"),
    )
    .await?;
    page.goto("/").await?;

    let controls = page.locator("#library-controls");
    controls
        .get_by_label("Filter by organization")
        .select_option(&[&t.org])
        .await?;
    controls
        .get_by_label("Filter by category")
        .select_option(&["Reports"])
        .await?;
    page.get_by_role(Role::new("searchbox").name("Search artifacts"))
        .fill("PW Reset")
        .await?;
    controls
        .get_by_label("Sort artifacts")
        .select_option(&["title"])
        .await?;
    let clear = page.locator("[data-reset-filters]");
    expect(&clear).to_be_visible().await?;
    clear.click().await?;

    expect(&page.get_by_role(Role::new("searchbox").name("Search artifacts")))
        .to_have_value("")
        .await?;
    expect(&controls.get_by_label("Filter by organization"))
        .to_have_value("all")
        .await?;
    expect(&controls.get_by_label("Filter by category"))
        .to_have_value("all")
        .await?;
    expect(&controls.get_by_label("Sort artifacts"))
        .to_have_value("recent")
        .await?;
    expect(&page.locator(r#"[data-filter-view="all"]"#))
        .to_have_attribute("aria-pressed", "true")
        .await?;
    expect(&clear).to_be_hidden().await?;
    assert_eq!(page.page_errors(), Vec::<String>::new());
    Ok(())
}

async fn tablet_toolbar(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let other_org = format!("{}-other", t.org);
    let created = t
        .api(
            "POST",
            "/settings/orgs",
            Some(json!({"name": other_org, "label": "Playwright other organization"})),
        )
        .await?;
    assert!(
        [200, 201, 400].contains(&created.status),
        "{}",
        created.status
    );
    let key_response = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({"clientId": format!("pw-tablet-{}", now_millis()), "org": other_org, "label": "playwright tablet"})),
        )
        .await?;
    let other_key = secret(&key_response.json());
    t.publish(
        &other_key,
        &format!("PW Tablet {other_org}"),
        "<!doctype html><h1>other</h1>",
        Some("Specs"),
    )
    .await?;
    t.publish(
        &key,
        &format!("PW Tablet {}", t.org),
        "<!doctype html><h1>tablet</h1>",
        Some("Reports"),
    )
    .await?;
    page.set_viewport(768, 1024, 1.0).await?;
    page.goto("/").await?;

    let organization = page
        .locator("#library-controls")
        .get_by_label("Filter by organization");
    let category = page
        .locator("#library-controls")
        .get_by_label("Filter by category");
    expect(&organization).to_be_visible().await?;
    expect(&category).to_be_visible().await?;
    organization.select_option(&[&t.org]).await?;
    category.select_option(&["Reports"]).await?;
    expect(&organization).to_have_value(t.org.as_str()).await?;
    expect(&category).to_have_value("Reports").await
}

async fn organization_scopes_categories(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let org = &t.org;
    let destination = org.replacen("pwtest-", "pwcat-", 1);
    let title = format!("PW Category move {org}");
    let created = t
        .api(
            "POST",
            "/settings/orgs",
            Some(json!({"name": destination, "label": "Playwright category destination"})),
        )
        .await?;
    assert!([200, 201].contains(&created.status), "{}", created.status);
    t.api(
        "POST",
        &format!("/settings/orgs/{org}/categories"),
        Some(json!({"name": "Source only"})),
    )
    .await?;
    t.api(
        "POST",
        &format!("/settings/orgs/{destination}/categories"),
        Some(json!({"name": "Destination only"})),
    )
    .await?;
    t.publish(
        &key,
        &title,
        "<!doctype html><h1>move</h1>",
        Some("Source only"),
    )
    .await?;
    page.goto("/").await?;

    let organization = page
        .locator("#library-controls")
        .get_by_label("Filter by organization");
    let filter_category = page
        .locator("#library-controls")
        .get_by_label("Filter by category");
    organization.select_option(&[&destination]).await?;
    expect(&filter_category.locator("option"))
        .to_have_texts(&["All categories", "Destination only (0)", "+ Category"])
        .await?;
    filter_category
        .select_option(&["__create_category__"])
        .await?;
    page.locator("#category-dialog")
        .get_by_label("Category name")
        .fill("Filter created")
        .await?;
    page.locator("#category-dialog")
        .get_by_role(Role::new("button").name("Create category"))
        .click()
        .await?;
    expect(&filter_category.locator(r#"option[value="Filter created"]"#))
        .to_have_text("Filter created (0)")
        .await?;

    organization.select_option(&[org]).await?;
    let card = page.locator(".card").filter_has_text(&title);
    card.get_by_role(Role::new("button").name(format!("More actions for {title}")))
        .click()
        .await?;
    card.get_by_label(format!("Move {title} to another organization"))
        .select_option(&[&destination])
        .await?;
    let card_category = card.get_by_label(format!("Change category for {title}"));
    expect(&card_category.locator("option"))
        .to_have_texts(&[
            "Uncategorized",
            "Destination only",
            "Filter created",
            "+ Category",
        ])
        .await?;
    card_category
        .select_option(&["__create_category__"])
        .await?;
    page.locator("#category-dialog")
        .get_by_label("Category name")
        .fill("Move created")
        .await?;
    page.locator("#category-dialog")
        .get_by_role(Role::new("button").name("Create category"))
        .click()
        .await?;
    expect(&card_category).to_have_value("Move created").await?;
    expect(&card.locator(".move-question"))
        .to_contain_text(format!("to {destination} in Move created"))
        .await?;
    card.get_by_role(Role::new("button").exact_name("Move"))
        .click()
        .await?;
    expect(
        &page
            .locator(&format!(r#".card[data-org="{destination}"]"#))
            .filter_has_text(&title),
    )
    .to_contain_text("Move created")
    .await
}

async fn back_link_restores_scroll(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let title = format!("PW Return {}", t.org);
    let titles: Vec<String> = (0..4).map(|index| format!("{title} {index}")).collect();
    let html: Vec<String> = (0..4)
        .map(|index| format!("<!doctype html><h1>{index}</h1>"))
        .collect();
    // Published concurrently, as `Promise.all` did.
    let (zero, one, two, three) = tokio::join!(
        t.publish(&key, &titles[0], &html[0], None),
        t.publish(&key, &titles[1], &html[1], None),
        t.publish(&key, &titles[2], &html[2], None),
        t.publish(&key, &titles[3], &html[3], None),
    );
    for published in [zero, one, two, three] {
        published?;
    }
    page.set_viewport(900, 320, 1.0).await?;
    page.goto("/").await?;
    expect(&page.get_by_text(format!("{title} 0")))
        .to_be_visible()
        .await?;
    page.evaluate("window.scrollTo(0, 280)").await?;
    let before = page.evaluate("window.scrollY").await?;

    let open = page
        .locator(".card")
        .filter_has_text(format!("{title} 0"))
        .get_by_role(Role::new("link").exact_name("Open"));
    let viewer_href = open.get_attribute("href").await?.unwrap_or_default();
    open.click().await?;
    expect(&page.locator(".vhome")).to_be_visible().await?;
    page.locator(".vhome").click().await?;
    expect(&page.locator(".collection-tools"))
        .to_be_visible()
        .await?;
    page.wait_for_function("(expected) => window.scrollY >= expected - 4", &[before])
        .await?;

    open.click().await?;
    expect(&page.locator(".vhome")).to_be_visible().await?;
    page.go_back().await?;
    expect(&page.locator(".collection-tools"))
        .to_be_visible()
        .await?;
    assert_eq!(
        page.evaluate(r#"sessionStorage.getItem("artifact-library-return")"#)
            .await?,
        Value::Null
    );

    page.goto(&viewer_href).await?;
    page.locator(".vhome").click().await?;
    expect(&page.locator(".collection-tools"))
        .to_be_visible()
        .await?;
    assert_eq!(page.evaluate("window.scrollY").await?, json!(0));
    Ok(())
}
