# syntax=docker/dockerfile:1.7
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Neil Blackman
#
# Network-isolated HTML-to-PNG preview renderer: a static Rust binary driving Chrome for Testing
# `chrome-headless-shell` over a DevTools pipe. No Node, no Playwright.
#
# Every base is digest-pinned and the browser archive is checked against a pinned SHA-256, so a
# tag update or a changed download cannot silently change the image.

# Same builder as the server image (../Dockerfile).
FROM rust:1.97.1-alpine3.23@sha256:c4a364ddbf684fe038e6fa6a4f25b30c8dc85247423e0e660676ece0d17be4a2 AS builder

RUN apk add --no-cache build-base \
    && rustup target add x86_64-unknown-linux-musl

WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY cdp ./cdp
ENV RUSTUP_TOOLCHAIN=1.97.1 \
    RUSTFLAGS="-C strip=symbols"
RUN cargo build --release --locked --target x86_64-unknown-linux-musl --bin artifact-preview

# Download and verify the browser in a throwaway stage, so curl and unzip stay out of the image.
FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac AS browser

# Chrome for Testing, Stable channel on 2026-10-22. To upgrade, change both values together:
#   curl -fsSLO https://storage.googleapis.com/chrome-for-testing-public/<version>/linux64/chrome-headless-shell-linux64.zip
#   sha256sum chrome-headless-shell-linux64.zip
ARG CHROME_VERSION=156.0.8078.47
ARG CHROME_SHA256=0d5acc62c3e213f8c87d3c298d66ffcf9a236d12ea4b9f85318bf85dbc279daa

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL --retry 3 -o /tmp/chrome-headless-shell.zip \
        "https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/linux64/chrome-headless-shell-linux64.zip" \
    && echo "${CHROME_SHA256}  /tmp/chrome-headless-shell.zip" | sha256sum -c - \
    && unzip -q /tmp/chrome-headless-shell.zip -d /opt \
    && mv /opt/chrome-headless-shell-linux64 /opt/chrome-headless-shell \
    && rm /tmp/chrome-headless-shell.zip \
    && chmod -R a+rX,go-w /opt/chrome-headless-shell

FROM ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac

ARG VERSION=dev
ARG REVISION=unknown
ARG CREATED=1970-01-01T00:00:00Z
ARG SOURCE=https://github.com/AgentShelf-OSS/artifact-mcp
LABEL org.opencontainers.image.title="artifact-preview" \
      org.opencontainers.image.description="Network-isolated HTML-to-PNG preview renderer for artifact-mcp" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.source="$SOURCE" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$REVISION" \
      org.opencontainers.image.created="$CREATED"

# The shared libraries chrome-headless-shell links against (readelf NEEDED; the archive bundles
# its own GL, Vulkan and SwiftShader), fontconfig, the fonts the Playwright image and CT222 ship,
# and tini to reap re-parented browser processes when the container runs without `--init`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libatspi2.0-0t64 libdbus-1-3 libexpat1 \
        libgbm1 libglib2.0-0t64 libnspr4 libnss3 libudev1 libx11-6 libxcb1 libxcomposite1 \
        libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 \
        fontconfig fonts-freefont-ttf fonts-ipafont-gothic fonts-liberation fonts-noto-color-emoji \
        fonts-tlwg-loma-otf fonts-unifont fonts-wqy-zenhei \
        tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 65532 artifact-preview \
    && useradd --system --uid 65532 --gid 65532 --home-dir /nonexistent --no-create-home \
        --shell /usr/sbin/nologin artifact-preview

COPY --from=browser /opt/chrome-headless-shell /opt/chrome-headless-shell
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/artifact-preview /usr/local/bin/artifact-preview

ENV PORT=3000 \
    RENDER_TIMEOUT_MS=7000 \
    CHROME_HEADLESS_SHELL=/opt/chrome-headless-shell/chrome-headless-shell \
    TMPDIR=/tmp

USER 65532:65532
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/artifact-preview", "healthcheck"]
# `tini -s` also works under compose's `init: true`, where it is not PID 1.
ENTRYPOINT ["/usr/bin/tini", "-s", "--", "/usr/local/bin/artifact-preview"]
