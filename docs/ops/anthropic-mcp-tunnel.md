# Anthropic MCP Tunnel runbook

Artifact MCP ships an opt-in `anthropic-tunnel` Compose profile for Anthropic's MCP Tunnels
research preview. The normal `docker compose up` deployment and its direct Streamable HTTP endpoint
are unchanged. Enabling this profile starts Anthropic's inner-TLS proxy plus `cloudflared`; neither
service publishes a host port.

This is not the Cloudflare Tunnel used to serve the human gallery. Anthropic MCP Tunnels are a
separate, Anthropic-managed research preview for reaching private MCP servers from Managed Agents
or the Messages API.

## Preview and security boundaries

- Access must be requested from Anthropic. The preview is supplied as-is, may change or end, and
  has no uptime, support, or continuity commitment.
- The connector makes outbound-only connections. Allow `api.anthropic.com:443` during
  provisioning and Cloudflare tunnel-edge traffic on TCP/UDP 7844 at runtime.
- Anthropic authenticates the tunnel transport, and the proxy terminates a separate inner TLS
  layer. Cloudflare can observe connection metadata but cannot read MCP payloads.
- The tunnel does not authenticate to Artifact MCP. Every caller must still send an Artifact MCP
  API key or OAuth access token. Keep the server's normal org, role, and scope checks enabled.
- The tunnel token and TLS private key together can impersonate the proxy. They are file-backed
  Compose secrets, ignored by Git and the Docker build context, and mounted only into the component
  that needs each secret.
- Both vendor images are digest-pinned. The containers run as UID 65532, read-only, with all Linux
  capabilities dropped, no privilege escalation, bounded resources, and bounded logs.

Primary vendor references:

- [MCP Tunnels overview](https://platform.claude.com/docs/en/agents-and-tools/mcp-tunnels/overview)
- [Docker Compose deployment](https://platform.claude.com/docs/en/agents-and-tools/mcp-tunnels/deploy-compose)
- [Security guidance](https://platform.claude.com/docs/en/agents-and-tools/mcp-tunnels/security)
- [Troubleshooting](https://platform.claude.com/docs/en/agents-and-tools/mcp-tunnels/troubleshooting)

## Provision

Use Anthropic's programmatic Workload Identity Federation flow when the host has an eligible OIDC
identity. Otherwise use the Console's manual flow. Both flows must produce these runtime files:

```text
ops/anthropic-tunnel/data/
├── tunnel-token
├── tls.crt
└── tls.key
```

The provisioning-only CA key does not belong in the runtime directory. Keep it in the normal
encrypted secret store. Make the runtime files readable only by the non-root tunnel UID:

```bash
sudo chown 65532:65532 ops/anthropic-tunnel/data/{tunnel-token,tls.crt,tls.key}
sudo chmod 600 ops/anthropic-tunnel/data/{tunnel-token,tls.key}
sudo chmod 644 ops/anthropic-tunnel/data/tls.crt
```

Copy the fail-closed proxy example and replace its tunnel domain:

```bash
cp ops/anthropic-tunnel/mcp-proxy.example.yaml ops/anthropic-tunnel/mcp-proxy.yaml
```

The shipped `artifacts` route resolves to `http://artifact-mcp:3480`; paths are forwarded
unchanged, so the private MCP URL is:

```text
https://artifacts.<your-tunnel-domain>/mcp
```

The example limits proxy targets to Docker's `172.16.0.0/12` bridge range. Narrow that CIDR if the
production Compose network has a pinned subnet. Do not set `upstream.disable_ip_validation`.

If the files live elsewhere, set only their paths in `.env`:

```dotenv
ANTHROPIC_TUNNEL_CONFIG_FILE=./ops/anthropic-tunnel/mcp-proxy.yaml
ANTHROPIC_TUNNEL_TOKEN_FILE=./ops/anthropic-tunnel/data/tunnel-token
ANTHROPIC_TUNNEL_TLS_CERT_FILE=./ops/anthropic-tunnel/data/tls.crt
ANTHROPIC_TUNNEL_TLS_KEY_FILE=./ops/anthropic-tunnel/data/tls.key
```

Do not put the tunnel token, TLS key, Anthropic API key, or Artifact MCP bearer token in `.env`.
The runtime profile uses file-backed secrets, and the validation command reads its credentials
directly from operator-owned files.

## Start and validate before cutover

Start the profile without changing the existing public route:

```bash
npm run tunnel:start
npm run tunnel:status
```

`tunnel:status` reports three independent signals:

1. Artifact MCP's database and storage health check.
2. The Anthropic proxy process.
3. `cloudflared tunnel ready`, which requires an active tunnel-edge connection.

Then validate from Anthropic's side. Use a dedicated, least-privilege Artifact MCP reader token for
the probe. The command makes one Messages API request and allows only the read-only
`list_artifacts` tool:

```bash
export ANTHROPIC_TUNNEL_MCP_URL=https://artifacts.YOUR_TUNNEL_DOMAIN/mcp
export ANTHROPIC_TUNNEL_TEST_MODEL=YOUR_APPROVED_MODEL
export ANTHROPIC_API_KEY_FILE=/run/secrets/anthropic-api-key
export ANTHROPIC_TUNNEL_UPSTREAM_TOKEN_FILE=/run/secrets/artifact-mcp-reader-token
npm run tunnel:validate
```

A pass proves that the tunnel is registered in the correct Anthropic workspace, inner TLS works,
the route reaches Artifact MCP, upstream bearer authentication succeeds, and a tool result returns
to Anthropic. It is stronger than a direct `curl`, because private tunnel hostnames are intended to
be called by Managed Agents or the Messages API.

Keep the existing public `/mcp` path during the pilot and observation window. Only after the remote
probe and a real client workflow pass should an operator remove the public `/mcp` bypass policy or
edge route. Leave the gallery's public hostname and catch-all Access application intact if people
still use the web UI. That external policy change is intentionally not automated by this repo.

## Monitoring

- Run `npm run tunnel:status` after deploys and host restarts.
- Alert separately on Artifact MCP's existing `/health` and Prometheus rules, the tunnel
  container's Docker health, and a low-frequency Anthropic-side validation.
- Watch bounded logs with
  `docker compose --profile anthropic-tunnel logs --tail=100 anthropic-mcp-proxy anthropic-mcp-cloudflared`.
  The token is supplied by file and never appears in the command or Compose environment.
- Monitor certificate expiry without printing the private key:

  ```bash
  openssl x509 -checkend 2592000 -noout -in ops/anthropic-tunnel/data/tls.crt
  ```

  Exit 1 means the certificate expires within 30 days.

## Rotation

For a tunnel-token rotation, retrieve the replacement through the Anthropic Console or setup
component, atomically replace `tunnel-token`, restore ownership/mode, then recreate only
`anthropic-mcp-cloudflared`:

```bash
docker compose --profile anthropic-tunnel up -d --force-recreate anthropic-mcp-cloudflared
npm run tunnel:status
npm run tunnel:validate
```

For certificate rotation, register the replacement CA first, atomically replace `tls.crt` and
`tls.key`, then recreate the proxy and cloudflared containers. Keep the old registered CA until
validation succeeds.

Rotate the probe's Artifact MCP credential through Artifact MCP's key management independently.
The tunnel is transport, not upstream authorization.

## Failure isolation

| Signal | Likely layer | First check |
|---|---|---|
| Artifact MCP health fails | database, storage, or app process | `docker compose logs --tail=100 artifact-mcp` |
| Proxy is not running | config, TLS file, permissions, or route validation | proxy logs; exact `tunnel_domain`; UID 65532 file access |
| Tunnel readiness fails | token or outbound TCP/UDP 7844 | cloudflared logs; firewall; token rotation state |
| Local checks pass but remote validation fails | workspace registration, CA, route hostname, or upstream auth | Anthropic Console registration, then proxy logs |
| Remote call returns authorization failure | Artifact MCP token/scope/org | dedicated reader credential and OAuth/API-key policy |
| Proxy reports IP validation failure | Docker subnet is outside the allowlist | pin/narrow the real subnet; never disable validation |

## Rollback

While the original public MCP path remains enabled, rollback is one command:

```bash
npm run tunnel:rollback
```

It confirms the default Artifact MCP service is healthy, then stops only the Anthropic proxy and
cloudflared services. It does not delete credentials or archive the Anthropic tunnel.

If public `/mcp` ingress was removed after cutover, restore that operator-owned edge policy first,
then run the rollback command. For decommissioning, also detach agent/API configurations, archive
the tunnel in Anthropic, and securely remove its token and TLS/CA keys according to Anthropic's
security runbook.
