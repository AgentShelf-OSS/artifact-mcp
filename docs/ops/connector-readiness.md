# Artifact MCP connector-readiness checklist

Run this checklist for each connector release and attach the evidence to the release or rollout
ticket. A checked item needs a link to test output, configuration review, or an operational owner;
the checklist itself is not evidence.

## 2026-07-29 native Rust / MCP 2026 release evidence

- Release candidate: cache-free `Dockerfile.rust` build, promoted only after the isolated Rust
  server passed the browser, protocol, parity, security, and operations gates below.
- Protocol: both MCP versions, discovery, resources, typed outputs, the review App, durable tasks,
  and the deliberately absent Skills extension are covered by the 472-test native suite and the
  black-box conformance replay.
- Authorization: the 212-test Node suite, 472-test Rust suite, and 52-test cross-runtime Playwright
  suite cover admin/recorded-owner controls and concealed same-org/cross-org denials.
- Browser: the isolated Rust release candidate completed the admin and member release audit with
  zero console errors, page errors, or failed requests. The README screenshots are captures from
  that server using fictional organizations and artifacts.
- Operations: `cargo fmt`, Clippy with warnings denied, `npm audit --omit=dev`, Compose validation,
  Prometheus rule validation, `/health`, `/metrics`, backup verification, rollback capture, and
  live legacy/modern MCP smoke results are recorded in the deployment log and GitHub issue
  closure comments.
- Enterprise OAuth remains intentionally disabled in production pending the IdP/topology decision
  tracked by PBI-074; the optional OAuth implementation and its fail-closed behavior are tested,
  but this release does not claim a configured enterprise connector.

## Protocol and discovery

- [ ] Both `2025-06-18` and `2026-07-28` conformance suites pass.
- [ ] `server/discover` reports the deployed server version, supported versions, cache scope, and
      only configured extensions.
- [ ] Modern `MCP-Protocol-Version`, `Mcp-Method`, and encoded `Mcp-Name` headers are validated
      against per-request metadata.
- [ ] Tools, resources, templates, typed outputs, and MCP App negotiation match the published
      schemas; legacy responses are unchanged.

## Authentication and authorization

- [ ] The protected-resource metadata URL is reachable from the connector.
- [ ] OAuth issuer, exact audience, JWKS URL, allowed algorithms, maximum token lifetime, and
      clock tolerance match the authorization-server configuration.
- [ ] Required scopes are least-privilege and a missing scope produces HTTP 403 plus the correct
      challenge.
- [ ] API-key compatibility is intentionally enabled or disabled and rollback credentials are
      tested.
- [ ] Admin-or-owner rules pass for visibility, sharing, and deletion; cross-org access remains
      concealed.
- [ ] Enterprise-managed authorization remains disabled until PBI-074's IdP/topology ADR is
      approved.

## Privacy and security

- [ ] MCP App CSP remains deny-by-default with no undeclared permissions.
- [ ] Artifact HTML stays inside the existing sandbox and is not copied into App HTML.
- [ ] Logs and metrics contain no tokens, Authorization headers, identities, organizations,
      artifact contents, raw arguments, raw resource URIs, or JSON-RPC ids.
- [ ] `/metrics` is reachable by Prometheus but excluded from the public `/mcp` bypass rule.
- [ ] Invalid auth, scope, protocol metadata, payload sizes, and output schemas fail closed.

## Testing and operations

- [ ] `cargo fmt --check`, clippy with warnings denied, the complete Rust suite, and the complete
      Node parity suite pass.
- [ ] Browser QA passes for the MCP review App at desktop and mobile sizes with no console errors.
- [ ] Prometheus scrape succeeds and `ops/prometheus/artifact-mcp-alerts.yml` passes `promtool`.
- [ ] A success, authentication failure, authorization failure, validation failure, server
      failure, cancellation, and output-validation failure can be observed with safe dimensions.
- [ ] On-call ownership, support intake, status communication, and the observability runbook are
      linked from the rollout ticket.

## Rollout and rollback

- [ ] Deploy to a canary connector or tenant first and record baseline error/latency metrics.
- [ ] Confirm legacy traffic and modern traffic both remain healthy during the observation window.
- [ ] Record the previous image digest, database backup/restore point, and exact rollback command.
- [ ] Roll back on output-validation failure, sustained server errors, auth-wide failure, or a
      security/privacy regression.
- [ ] After rollback, verify `/health`, one legacy tool call, one modern discovery request, one
      scoped OAuth call, and metrics ingestion.
