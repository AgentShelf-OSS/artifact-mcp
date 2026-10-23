// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-place-sync` (ADR-0022): the saved Listen place follows the signed-in viewer to a
//! second browser, keeps the new speeds, and "Finished" clears the server copy too, from
//! `playwright/tests/reader-place-sync.cjs`.

use std::time::Duration;

use browser_tests::reader::{Audio, ReaderCtx, ReaderFixture};
use browser_tests::{Error, Page, expect};
use serde_json::{Value, json};

use crate::common::{checkpoint, errors, num, ordered, preferences_script, same, wait};

fn sentence(word: &str) -> String {
    format!(
        "{word} paragraph begins here. It keeps going with a second sentence. A third sentence closes it."
    )
}

/// The paused place keeps the new speed, the second paragraph, an offset and a `savedAt`.
fn check_saved(saved: &Value) {
    assert_eq!(
        saved["current"]["rate"],
        json!("2.5"),
        "new speed not kept in the checkpoint"
    );
    assert!(same(&saved["current"]["ordinal"], &json!(1)));
    assert!(num(&saved["current"]["offset"]) > 0.0, "no offset saved");
    assert!(
        saved["savedAt"].as_f64().is_some_and(f64::is_finite),
        "checkpoint has no savedAt"
    );
}

/// The server copy of the place (`GET /<id>/state/listen-place?scope=viewer`), or `null`.
async fn server_place(fixture: &ReaderFixture, id: &str) -> Result<Value, Error> {
    let response = fixture
        .viewer_client()
        .get(&format!("/{id}/state/listen-place?scope=viewer"))
        .await?;
    if response.status == 404 {
        return Ok(Value::Null);
    }
    assert_eq!(response.status, 200, "state read failed");
    Ok(response.json()["value"].clone())
}

/// Read the server copy up to 12 times, 750 ms apart, until `check` holds.
async fn poll_place(
    fixture: &ReaderFixture,
    id: &str,
    check: impl Fn(&Value) -> bool,
    label: &str,
) -> Result<Value, Error> {
    let mut last = Value::Null;
    for _ in 0..12 {
        let value = server_place(fixture, id).await?;
        if check(&value) {
            return Ok(value);
        }
        last = value;
        wait(750).await;
    }
    Err(Error::Assertion(format!(
        "{label} (last server value: {last})"
    )))
}

async fn open(
    t: &ReaderCtx,
    fixture: &ReaderFixture,
    url: &str,
    rate: &str,
) -> Result<Page, Error> {
    let page = t
        .page_with(Some((1200, 800)), &fixture.viewer_headers())
        .await?;
    page.add_init_script(&preferences_script("pocket_alba", rate))
        .await?;
    page.goto(url).await?;
    crate::common::open_reader(&page).await?;
    Ok(page)
}

async fn playing(page: &Page) -> Result<(), Error> {
    page.wait_for_function_with_timeout(
        "() => document.querySelector('.vreader').dataset.streamState === 'playing'",
        &[],
        Duration::from_secs(10),
    )
    .await?;
    Ok(())
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(20.0).latency(30)).await?;
    let html = format!(
        "<main><p id=a>{}</p><p id=b>{}</p><p id=c>{}</p></main>",
        sentence("Alpha"),
        sentence("Bravo"),
        sentence("Charlie")
    );
    let published = fixture.publish("place sync QA", &html).await?;
    let id = published.id.clone();
    fixture.assert_speech_routes(&id).await?;

    // First browser: a new speed, the second paragraph, then pause.
    let first = open(&t, &fixture, &published.url, "2.5").await?;
    let rates = first
        .locator("#vreader-rate")
        .evaluate(
            "select => [...select.querySelectorAll('option')].map(option => option.value)",
            Value::Null,
        )
        .await?;
    for rate in [
        "0.75", "0.8", "0.9", "1", "1.1", "1.25", "1.5", "1.75", "2", "2.5", "3",
    ] {
        assert!(
            rates
                .as_array()
                .is_some_and(|rates| rates.contains(&json!(rate))),
            "rate {rate} missing"
        );
    }
    assert_eq!(
        first.locator("#vreader-rate").input_value().await?,
        "2.5",
        "saved preference rate not applied"
    );
    first.locator("#vreader-play").click().await?;
    playing(&first).await?;
    first.locator("#vreader-next").click().await?;
    playing(&first).await?;
    wait(1500).await;
    first.locator("#vreader-play").click().await?;
    first
        .wait_for_function_with_timeout(
            "() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k && JSON.parse(localStorage.getItem(k)).current.ordinal === 1; }",
            &[],
            Duration::from_secs(5),
        )
        .await?;
    let paused = checkpoint(&first).await?;
    check_saved(&paused);
    // The upload is the paused place, or the same place as it settles: Pause saves at once, but
    // the audio clock runs on until `AudioContext.suspend()` resolves, so the shell's 2 s save
    // tick can store a slightly later offset (with a new `savedAt`), and a pause within 2 s of
    // the previous upload sends that settled copy instead. Either way the server must hold
    // exactly a place this browser saved after the pause.
    let mut last = Value::Null;
    let mut matched = None;
    for _ in 0..12 {
        let value = server_place(&fixture, &id).await?;
        let local = checkpoint(&first).await?;
        let server_saved_at = &value["pages"][""]["savedAt"];
        if let Some(saved) = [&paused, &local]
            .into_iter()
            .find(|saved| same(server_saved_at, &saved["savedAt"]))
        {
            matched = Some((value.clone(), saved.clone()));
            break;
        }
        last = value;
        wait(750).await;
    }
    let Some((uploaded, saved)) = matched else {
        return Err(Error::Assertion(format!(
            "paused place never reached the server (paused place: {paused}; last server value: {last})"
        )));
    };
    check_saved(&saved);
    let saved_at = saved["savedAt"].clone();
    assert!(
        same(
            &uploaded["pages"][""]["place"]["current"],
            &saved["current"]
        ),
        "{} != {}",
        uploaded["pages"][""]["place"]["current"],
        saved["current"]
    );
    assert_eq!(errors(&first), Vec::<String>::new());
    first.close().await;

    // Second browser: empty localStorage, another default speed. Play continues the first
    // browser's place.
    let second = open(&t, &fixture, &published.url, "1").await?;
    expect(&second.locator("#vreader-resume"))
        .timeout(Duration::from_secs(8))
        .to_be_visible()
        .await?;
    let cached = checkpoint(&second).await?;
    assert!(
        same(&cached["savedAt"], &saved_at),
        "server place not cached locally: {cached}"
    );
    fixture.worker().reset();
    second.locator("#vreader-play").click().await?;
    playing(&second).await?;
    let requests = fixture.worker().requests();
    let first_text = requests
        .first()
        .and_then(|request| request.body["text"].as_str())
        .unwrap_or("")
        .to_owned();
    assert!(
        first_text.starts_with("Bravo"),
        "Play did not continue from the synced place: {first_text:?}"
    );
    assert_eq!(
        second.locator("#vreader-rate").input_value().await?,
        "2.5",
        "synced speed not restored"
    );
    let restored = checkpoint(&second).await?;
    assert!(same(&restored["current"]["ordinal"], &json!(1)));
    let (restored_offset, saved_offset) = (
        num(&restored["current"]["offset"]),
        num(&saved["current"]["offset"]),
    );
    assert!(
        restored_offset >= saved_offset - 0.05,
        "restored offset {restored_offset} < {saved_offset}"
    );

    // Finishing clears both copies.
    fixture.worker().set_duration(0.3);
    second.locator("#vreader-next").click().await?;
    second
        .wait_for_function_with_timeout(
            "() => document.querySelector('#vreader-status').textContent === 'Finished'",
            &[],
            Duration::from_secs(10),
        )
        .await?;
    assert_eq!(
        checkpoint(&second).await?,
        Value::Null,
        "local place kept after Finished"
    );
    poll_place(
        &fixture,
        &id,
        |value| value.is_null() || value["pages"][""].is_null(),
        "server place kept after Finished",
    )
    .await?;
    assert_eq!(errors(&second), Vec::<String>::new());
    Ok(ordered(&[
        ("crossDeviceResume", json!(true)),
        ("newRatesInCheckpoint", json!(true)),
        ("finishedClearsServer", json!(true)),
    ]))
}
