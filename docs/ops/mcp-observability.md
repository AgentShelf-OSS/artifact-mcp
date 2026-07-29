# MCP observability runbook

Artifact MCP exposes Prometheus text metrics at `GET /metrics`. Keep the route behind the same
private ingress or access policy as the gallery; do not add it to the public `/mcp` bypass rule.
Responses are `no-store`.

Every MCP response includes a server-generated opaque `X-Request-Id`. The same id appears in the
structured `MCP request completed` log event. A client-supplied request id is never used for this
correlation field.

## Safe dimensions

Metrics and completion logs contain only:

- negotiated protocol (`2025-06-18`, `2026-07-28`, `unsupported`, or `unknown`);
- an allowlisted MCP method and operation class;
- an allowlisted tool name or a coarse resource kind;
- outcome, duration, and a bounded response-size band.

Unknown methods and names collapse to `unknown`. Artifact ids, resource URIs, JSON-RPC ids,
publisher identities, organizations, arguments, contents, credentials, tokens, and authorization
headers are never passed to the recorder.

Outcomes distinguish success, authentication failure, authorization failure, input/transport
validation failure, output validation failure, protocol error, server failure, and cancellation.
A request whose handler future or HTTP connection is abandoned is recorded as cancelled.

## Scrape and verify

Configure the existing Prometheus deployment to scrape `http://artifact-mcp:3480/metrics` over the
private Docker network. Verify:

```sh
curl --fail --silent --show-error http://127.0.0.1:3480/metrics
```

Load `ops/prometheus/artifact-mcp-alerts.yml` into the Prometheus rule paths and validate it with
the deployment's `promtool check rules` command before reloading Prometheus.

## Authentication failures

Check whether failures began after credential rotation, authorization-server maintenance, issuer
or audience changes, JWKS rotation, or API-key revocation. Correlate only by `X-Request-Id`; never
copy an Authorization header or token into a ticket or log query. A high failure ratio with a low
absolute rate is suppressed by the alert.

## Server errors

Break down `artifact_mcp_requests_total{outcome="server_failure"}` by safe `method` and `name`
labels, then use an opaque request id to inspect the matching structured event. Check SQLite,
artifact storage, preview integration, and downstream health without enabling argument logging.

## Latency

Use the duration histogram grouped by `operation`, `method`, or allowlisted `name`. Compare the
result-size band and storage/preview health. The default alert fires when aggregate p95 latency is
above two seconds for fifteen minutes; tune only after a representative production baseline.

## Output validation

Any output-validation failure is a server contract regression. Stop rollout, identify the
allowlisted tool name, reproduce against the conformance schema, and roll back if a forward fix is
not immediately safe. Never disable schema validation to clear the alert.

## Rollback

The telemetry recorder is in-process and does not change MCP response bodies. If metrics cause an
operational problem, stop scraping `/metrics` and revert the application image. Existing MCP
clients remain compatible throughout the rollback.
