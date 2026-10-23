// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Network isolation of the running binary: no page may reach a canary on this host, by IP
//! literal on loopback or on a LAN interface, or by name; `data:` resources still render.
//!
//! `src/tests.rs` repeats the vectors with each defence layer on its own and with none (the
//! control), which shows the vectors do reach the canary when nothing stops them.

mod support;

use std::time::Duration;

use artifact_preview::client::send;
use hyper::body::Bytes;
use hyper::{Method, StatusCode};
use support::vectors::{HOLD_LOAD, vectors};
use support::{Canary, RED_PNG_BASE64, Service, canary_hosts, chrome, decode_png, pixel};

const TIMEOUT: Duration = Duration::from_secs(20);

async fn render(service: &Service, html: &str, width: u32, height: u32) -> (StatusCode, Bytes) {
    let body = serde_json::json!({"html": html, "width": width, "height": height});
    let response = send(
        &service.authority,
        Method::POST,
        "/render",
        Some((
            "application/json",
            Bytes::from(serde_json::to_vec(&body).unwrap()),
        )),
        TIMEOUT,
    )
    .await
    .unwrap();
    (response.status, response.body)
}

#[tokio::test]
async fn no_vector_reaches_the_canary() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "isolation", &[]);
    let canary = Canary::start();
    let mut report = Vec::new();
    for host in canary_hosts() {
        for (name, page) in vectors(&host.to_string(), canary.tcp_port, canary.udp_port) {
            let before = canary.hits();
            let (status, _) = render(&service, &format!("{page}{HOLD_LOAD}"), 400, 300).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let after = canary.hits();
            report.push(format!(
                "{name}@{host}: HTTP {} canary {:?}",
                status.as_u16(),
                (after.0 - before.0, after.1 - before.1)
            ));
        }
    }
    eprintln!("{}", report.join("\n"));
    assert_eq!(
        canary.hits(),
        (0, 0),
        "canary log: {:?}",
        canary.log.lock().unwrap()
    );
}

#[tokio::test]
async fn data_resources_render() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "data-image", &[]);
    let blue_svg = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='4' height='4'%3E%3Crect width='4' height='4' fill='%230000ff'/%3E%3C/svg%3E";
    let page = format!(
        "<!doctype html><style>body{{margin:0;background:#fff}} div{{position:absolute;top:0;left:200px;width:100px;height:100px;background:url(\"{blue_svg}\")}}</style>\
         <img src=\"data:image/png;base64,{RED_PNG_BASE64}\" style=\"position:absolute;top:0;left:0;width:100px;height:100px\"><div></div>"
    );
    let (status, png) = render(&service, &page, 400, 300).await;
    assert_eq!(status, StatusCode::OK);
    let image = decode_png(&png);
    assert_eq!((image.0, image.1), (800, 600));
    // Device pixels are twice CSS pixels.
    assert_eq!(pixel(&image, 100, 100), [255, 0, 0, 255], "data: PNG <img>");
    assert_eq!(
        pixel(&image, 500, 100),
        [0, 0, 255, 255],
        "data: SVG CSS background"
    );
    assert_eq!(
        pixel(&image, 700, 500),
        [255, 255, 255, 255],
        "page background"
    );
}
