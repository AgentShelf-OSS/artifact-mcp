# Artifact MCP documentation

The root [README](../README.md) explains what Artifact MCP does and gets a local server running.
Use this index when you need the complete API, deployment, security, or maintenance details.

## Start here

- [Getting started](../GETTING_STARTED.md) takes a new installation from a local Docker run to a
  production deployment behind Cloudflare Access.
- [Configuration reference](configuration.md) documents every supported environment variable and
  the operational procedures tied to encrypted webhooks and persistent previews.
- [MCP API and tools](mcp-api.md) covers authentication, protocol negotiation, tools, resources,
  MCP Apps, and durable tasks.

## Product and design

- [Screenshot gallery](screenshots/README.md) contains the full product tour.
- [Comparison guide](comparison.md) explains when Artifact MCP is a better fit than a hosted page
  publisher or a self-hosted chat interface.
- [Frontend maintenance](frontend-maintenance.md) records the portal's visual contracts and checks.
- [Release notes](releases/README.md) summarize user-visible changes by release.

## Architecture and security

- [Architecture and routes](architecture.md) describes the production runtime, trust boundaries,
  HTTP routes, and source layout.
- [Security model](security.md) covers identity, tenant isolation, sandboxing, public shares,
  browser mutations, integrations, and known exclusions.
- [Domain context](../CONTEXT.md) defines stable terms, invariants, and module responsibilities.
- [Architecture decisions](adr/) preserve the reasons behind storage, sandboxing, modularity, and
  integration choices.
- [Security audit ledger](security-audit-ledger.md) tracks security review evidence.

## Deployment

- [Cloudflare deployment](DEPLOY-CLOUDFLARE.md) covers Access bootstrap, policy ownership,
  least-privilege credentials, cookie settings, and troubleshooting.
- [Release provenance](release-provenance.md) documents immutable image and release verification.
- [Durability](ops/durability.md) covers backups and recovery for SQLite and artifact files.
- [Ingress controls](ops/ingress-controls.md) documents admission limits and trusted proxy rules.

## Operations

- [Connector readiness](ops/connector-readiness.md)
- [MCP observability](ops/mcp-observability.md)
- [Discord durable delivery](ops/discord-durable-delivery.md)
- [Discord Gateway boundary decision](adr/0006-discord-gateway-client-boundary.md)
- [Anthropic MCP Tunnel](ops/anthropic-mcp-tunnel.md)

Some runbooks target optional integrations. The core server only needs the Rust container, SQLite,
artifact files, and one configured publishing credential.
