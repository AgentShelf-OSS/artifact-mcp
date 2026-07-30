# Origin ingress controls

Artifact MCP enforces request admission at the origin even when Cloudflare or a reverse proxy is
present. Configure equivalent edge limits for bandwidth savings, but do not rely on them as the
only protection: direct-origin traffic and proxy configuration drift are in scope.

The listener rejects excessive HTTP/1 headers before routing and applies a header-read deadline.
The application then bounds URI/header size, declared body length, streaming JSON/MCP body reads,
JSON depth/nodes/container width/batch size, whole-request concurrency, unsafe-request
concurrency, separate MCP/upload/feedback/admin request budgets, and preview queue depth.
Read-only handlers have a separate response deadline. On expiry the client receives `408`, but
the underlying task retains its admission permit until it really finishes, so an uncancellable
dependency cannot turn a timeout into excess concurrent work. Durable mutation handlers do not
use that deadline: once admitted, they run to completion because client timeout cannot safely
imply that SQLite/filesystem work did not commit.

Every application-level rejection has a stable JSON envelope (JSON-RPC for `/mcp`). Slow **body**
reads return `408`; byte and complexity limits return `413`; URI/header limits return `414`/`431`;
rates return `429` with a bounded `Retry-After`; request-admission pressure returns `503` with
`Retry-After`. Preview queue pressure is optional work: it is counted and the calling view falls
back to the existing placeholder rather than failing an already-valid durable mutation. A header-read timeout occurs before an HTTP request exists, so Hyper closes that
slowloris connection rather than promising an HTTP `408` it cannot safely write.

Pre-auth and failed-auth limits are opaque source-only hashes; rotating an invalid bearer never
creates another bucket. `/mcp` adds source-plus-verified-publisher budgets only after successful
authentication, including a stricter artifact-content-write budget after bounded JSON-RPC parsing
identifies a publish/update tool. Once Access resolves a browser viewer, the origin adds a
tenant-plus-verified-email-plus-source budget; raw cookies and Access headers are never limiter
keys. Likewise, a share token becomes a source-plus-token principal only after the share store
successfully resolves and authorizes it. No bucket identity is logged or exported as a metric label. Invalid candidate
keys and shares use the same source-scoped denial path, so a limit response cannot reveal whether
a key or share exists. `CF-Connecting-IP` is accepted only when the socket peer belongs to
`TRUSTED_PROXY_CIDRS`; `X-Forwarded-For` is never position-guessed and is ignored.

`/metrics` exposes only fixed rejection reasons, alongside existing MCP telemetry. It contains no
token, tenant, URI, artifact, user, or source label. Tune defaults only from bounded-load evidence;
raising a global capacity must be paired with database, storage, and renderer capacity review.
