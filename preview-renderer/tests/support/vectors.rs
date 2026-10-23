// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Pages that each try one way to reach the network. `{H}` is the canary host (an IP literal or
//! `localhost`), `{P}` its TCP port and `{U}` its UDP port. Every vector must give the canary
//! zero connections and zero datagrams.
#![allow(dead_code)]

/// `(name, page)` for one canary host.
pub fn vectors(host: &str, tcp_port: u16, udp_port: u16) -> Vec<(&'static str, String)> {
    let host_for_url = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    TEMPLATES
        .iter()
        .map(|(name, template)| {
            let page = template
                .replace("{H}", &host_for_url)
                .replace("{P}", &tcp_port.to_string())
                .replace("{U}", &udp_port.to_string());
            (*name, page)
        })
        .collect()
}

/// Keeps the document from finishing its `load` for 500 ms, so requests started during parsing
/// have time to reach the canary if anything lets them out.
pub const HOLD_LOAD: &str = "<script>addEventListener('load', () => { const until = Date.now() + 500; while (Date.now() < until) {} });</script>";

const TEMPLATES: &[(&str, &str)] = &[
    // The thirteen vectors the acceptance gate names.
    ("img", r#"<img src="http://{H}:{P}/img">"#),
    (
        "css-import",
        r#"<style>@import url("http://{H}:{P}/import.css");</style><p>x</p>"#,
    ),
    (
        "font-face",
        r#"<style>@font-face{font-family:canary;src:url("http://{H}:{P}/font.woff2")} p{font-family:canary}</style><p>text</p>"#,
    ),
    (
        "fetch",
        r#"<script>fetch("http://{H}:{P}/fetch", {mode: "no-cors"}).catch(() => {});</script>"#,
    ),
    (
        "xhr-async",
        r#"<script>try { const x = new XMLHttpRequest(); x.open("GET", "http://{H}:{P}/xhr"); x.send(); } catch (e) {}</script>"#,
    ),
    (
        "xhr-sync",
        r#"<script>try { const x = new XMLHttpRequest(); x.open("GET", "http://{H}:{P}/xhr-sync", false); x.send(); } catch (e) {}</script>"#,
    ),
    (
        "send-beacon",
        r#"<script>try { navigator.sendBeacon("http://{H}:{P}/beacon", "data"); } catch (e) {}</script>"#,
    ),
    (
        "websocket",
        r#"<script>try { new WebSocket("ws://{H}:{P}/ws"); } catch (e) {}</script>"#,
    ),
    ("iframe", r#"<iframe src="http://{H}:{P}/frame"></iframe>"#),
    (
        "prefetch",
        r#"<link rel="prefetch" href="http://{H}:{P}/prefetch">"#,
    ),
    (
        "form-submit",
        r#"<form id="f" method="POST" action="http://{H}:{P}/form"><input name="a" value="b"></form><script>document.getElementById("f").submit();</script>"#,
    ),
    (
        "location",
        r#"<script>location = "http://{H}:{P}/location";</script>"#,
    ),
    (
        "window-open",
        r#"<script>window.open("http://{H}:{P}/popup");</script>"#,
    ),
    (
        "service-worker",
        r#"<script>
          try { navigator.serviceWorker.register("http://{H}:{P}/sw.js").catch(() => {}); } catch (e) {}
          try { const r = Object.getOwnPropertyDescriptor(ServiceWorkerContainer.prototype, "register").value; r.call(navigator.serviceWorker, "http://{H}:{P}/sw2.js").catch(() => {}); } catch (e) {}
        </script>"#,
    ),
    // Further vectors.
    ("https-img", r#"<img src="https://{H}:{P}/tls">"#),
    (
        "localhost-name",
        r#"<img src="http://localhost:{P}/localhost">"#,
    ),
    (
        "script-src",
        r#"<script src="http://{H}:{P}/script.js"></script>"#,
    ),
    (
        "stylesheet",
        r#"<link rel="stylesheet" href="http://{H}:{P}/style.css">"#,
    ),
    (
        "css-background",
        r#"<div style="width:10px;height:10px;background:url(http://{H}:{P}/bg.png)"></div>"#,
    ),
    (
        "preconnect",
        r#"<link rel="preconnect" href="http://{H}:{P}">"#,
    ),
    (
        "dns-prefetch",
        r#"<link rel="dns-prefetch" href="http://canary.invalid:{P}">"#,
    ),
    (
        "meta-refresh",
        r#"<meta http-equiv="refresh" content="0;url=http://{H}:{P}/refresh">"#,
    ),
    (
        "anchor-ping",
        r##"<a id="a" href="#x" ping="http://{H}:{P}/ping">x</a><script>document.getElementById("a").click();</script>"##,
    ),
    (
        "event-source",
        r#"<script>try { new EventSource("http://{H}:{P}/sse"); } catch (e) {}</script>"#,
    ),
    (
        "object",
        r#"<object data="http://{H}:{P}/object"></object>"#,
    ),
    (
        "video",
        r#"<video src="http://{H}:{P}/video" autoplay muted></video>"#,
    ),
    (
        "svg-image",
        r#"<svg width="10" height="10"><image href="http://{H}:{P}/svg.png" width="10" height="10"/></svg>"#,
    ),
    (
        "dynamic-import",
        r#"<script type="module">import("http://{H}:{P}/module.js").catch(() => {});</script>"#,
    ),
    (
        "worker-fetch",
        r#"<script>try { new Worker(URL.createObjectURL(new Blob(['fetch("http://{H}:{P}/worker").catch(() => {})'], {type: "text/javascript"}))); } catch (e) {}</script>"#,
    ),
    (
        "shared-worker-fetch",
        r#"<script>try { new SharedWorker(URL.createObjectURL(new Blob(['fetch("http://{H}:{P}/shared").catch(() => {})'], {type: "text/javascript"}))); } catch (e) {}</script>"#,
    ),
    (
        "data-iframe-img",
        r#"<iframe src="data:text/html,<img src='http://{H}:{P}/data-frame'>"></iframe>"#,
    ),
    (
        "srcdoc-fetch",
        r#"<iframe srcdoc="<script>fetch('http://{H}:{P}/srcdoc').catch(() => {})</script>"></iframe>"#,
    ),
    (
        "speculation-prerender",
        r#"<script type="speculationrules">{"prerender": [{"source": "list", "urls": ["http://{H}:{P}/prerender"]}]}</script>"#,
    ),
    (
        "speculation-prefetch",
        r#"<script type="speculationrules">{"prefetch": [{"source": "list", "urls": ["http://{H}:{P}/spec-prefetch"]}]}</script>"#,
    ),
    (
        "webrtc-stun-udp",
        r#"<script>try { const pc = new RTCPeerConnection({iceServers: [{urls: "stun:{H}:{U}"}]}); pc.createDataChannel("d"); pc.createOffer().then((offer) => pc.setLocalDescription(offer)).catch(() => {}); } catch (e) {}</script>"#,
    ),
    (
        "webrtc-turn-tcp",
        r#"<script>try { const pc = new RTCPeerConnection({iceServers: [{urls: "turn:{H}:{P}?transport=tcp", username: "u", credential: "p"}]}); pc.createDataChannel("d"); pc.createOffer().then((offer) => pc.setLocalDescription(offer)).catch(() => {}); } catch (e) {}</script>"#,
    ),
    (
        "webtransport-udp",
        r#"<script>try { const t = new WebTransport("https://{H}:{U}/wt"); t.ready.catch(() => {}); t.closed.catch(() => {}); } catch (e) {}</script>"#,
    ),
];
