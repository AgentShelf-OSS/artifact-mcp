// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `artifact-preview`            serve on 0.0.0.0:$PORT
//! `artifact-preview healthcheck` exit 0 if GET http://127.0.0.1:$PORT/health returns 200
//! `artifact-preview smoke`       render a test page via $PREVIEW_RENDERER_URL (smoke.js), sending
//!                                $RENDER_TOKEN or $RENDER_TOKEN_FILE when one is set
#![deny(unsafe_code)]

use std::process::ExitCode;

use artifact_preview::client;
use artifact_preview::config::{Config, RenderToken, port_from};
use artifact_preview::server::Server;

const USAGE: &str = "usage: artifact-preview [healthcheck | smoke]";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // One thread: the browser's parent-death signal is tied to the spawning thread, and one
    // render at a time needs no more.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("[artifact-preview] cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => runtime.block_on(serve()),
        ["healthcheck"] => runtime.block_on(healthcheck()),
        ["smoke"] => runtime.block_on(smoke()),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

async fn serve() -> ExitCode {
    if std::process::id() == 1 {
        // Killed browser processes are re-parented to PID 1, which must reap them. Without an init
        // they would pile up as zombies until the pid limit stops every render.
        eprintln!(
            "[artifact-preview] refusing to run as PID 1: start it under an init (compose `init: true`, `docker run --init`)"
        );
        return ExitCode::FAILURE;
    }
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("[artifact-preview] {error}");
            return ExitCode::FAILURE;
        }
    };
    let server = match Server::bind(&config).await {
        Ok(server) => server,
        Err(error) => {
            eprintln!(
                "[artifact-preview] cannot listen on 0.0.0.0:{}: {error}",
                config.port
            );
            return ExitCode::FAILURE;
        }
    };
    println!("[artifact-preview] listening on 0.0.0.0:{}", config.port);
    tokio::select! {
        () = server.run() => ExitCode::SUCCESS,
        signal = shutdown_signal() => {
            // Dropping the runtime drops any in-flight render, whose `Drop` kills the browser's
            // process group and removes its temporary directory.
            if let Err(error) = signal {
                eprintln!("[artifact-preview] signal handling failed: {error}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
    }
}

async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }
    Ok(())
}

async fn healthcheck() -> ExitCode {
    let port = std::env::var("PORT").ok();
    let Ok(port) = port_from(port.as_deref()) else {
        return ExitCode::FAILURE;
    };
    if client::healthcheck(port).await {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn smoke() -> ExitCode {
    let base_url =
        std::env::var("PREVIEW_RENDERER_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let token = match RenderToken::from_env() {
        Ok(token) => token,
        Err(error) => {
            eprintln!("preview renderer smoke test failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    match client::smoke(&base_url, token.as_ref()).await {
        Ok(bytes) => {
            println!("preview renderer smoke test passed ({bytes} bytes)");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("preview renderer smoke test failed: {error}");
            ExitCode::FAILURE
        }
    }
}
