# ADR-0004: Defer Skills over MCP until the extension and a target client are stable

- **Status:** Accepted
- **Date:** 2026-07-29
- **Decision:** Defer
- **Approved:** 2026-07-29 — product approval recorded; security gate accepted based on the
  deny-by-default design, absent production surface, and automated negative tests
- **Revisit:** When the conditions below are all satisfied

## Context

Artifact MCP could eventually distribute a small artifact-review skill beside its tools and
resources. That would teach a capable client how to combine read, revision, feedback, visibility,
share, and deletion operations without copying workflow instructions into every integration.

The proposed transport is
[SEP-2640: Skills Extension](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2640),
using the proposed `io.modelcontextprotocol/skills` extension and `skill://` resources. As of this
decision:

- there is **no accepted specification version**;
- SEP-2640 is open, labeled `draft`, unmerged, and has requested changes;
- the working repository explicitly describes itself as
  [experimental and not an official specification or recommendation](https://github.com/modelcontextprotocol/experimental-ext-skills);
- the proposal lists public forks/prototypes for several hosts, an internal non-public Claude Code
  prototype, and a prototype GitHub MCP Server implementation—not released client conformance;
- Anthropic's published Messages API MCP connector currently supports MCP tool calls only, not
  resources or skills; and
- OpenAI's published Codex/ChatGPT model supports local or plugin-bundled skills alongside MCP
  servers, but its documented MCP client surface does not advertise the proposed Skills extension.

The proposed SEP's current security guidance is valuable but not a production contract. It treats
remote skills as untrusted prompt-injection input and requires origin labeling, per-skill consent
for local execution or permissions, origin-scoped resource reads, collision-safe naming,
content-bound approvals, digest verification, and cache isolation.

## Compatibility gate

| Target surface | Public support checked on 2026-07-29 | Safe fallback today | Gate |
|---|---|---|---|
| Claude Code | SEP records only an internal, non-public prototype; no accepted client version | Existing Artifact MCP tool descriptions/resources; separately installed local skill if an operator chooses | Fail |
| Claude Managed Agents / Messages API | Published MCP connector supports tools only | Existing tools, including typed results and the read-only `list_artifacts` flow | Fail |
| Codex CLI / IDE / ChatGPT desktop | Published MCP support includes server instructions and tools; skills are local or plugin-bundled, not `skill://`-discovered | Local/plugin skill paired with the same authorized MCP tools | Fail |
| ChatGPT Work/web | Plugins can package skills and remote MCP tools together, but this is package distribution rather than Skills over MCP | A separately reviewed plugin package, if product later needs it | Fail |
| Legacy/generic MCP clients | No extension negotiation or skill lifecycle | Ignore the absent extension and continue using the frozen tools/resources contract | Pass as fallback only |

No target client has both a public supported release and a conformance path for SEP-2640. The
adoption gate therefore fails.

## Decision

Defer implementation. Artifact MCP will not:

- advertise `io.modelcontextprotocol/skills`;
- implement `skills/list`, `skills/get`, or `resources/directory/read`;
- serve `skill://` resources;
- add a production feature flag; or
- ship a skill whose instructions can affect authorization or host permissions.

The existing server tools, typed resources, MCP App, and server-authored tool descriptions remain
the cross-client fallback. Authorization stays exclusively in the server's admin/owner/org/scope
policy; no instruction layer can grant a capability.

Because the gate failed, this change deliberately contains no prototype. A draft-protocol spike
would add a moving wire contract and a high-risk prompt-injection surface without a supported
client on which to validate product value.

## Allowed future prototype

If the gate later passes, the first prototype must be one narrowly scoped `artifact-review` skill:

- instructions and examples only—no bundled executable scripts, hooks, or implicit local actions;
- references to existing Artifact MCP tools and resources rather than duplicated access rules;
- read/review/feedback workflow only by default, with destructive or visibility actions left to
  the server's existing authorization and the client's normal confirmation UX;
- explicit treatment of artifact HTML, metadata, feedback, and supporting skill resources as
  untrusted content, never as privileged instructions;
- no `allowed-tools` permission widening;
- origin-tagged resource reads bound to this Artifact MCP server; and
- digest-bound, inspectable approval that is revoked whenever any skill file changes.

The prototype remains non-production until product and security reviewers approve both the content
and the host behavior.

## Operational cost and rollback

Adoption would require extension negotiation, catalog caching, per-file digest verification,
origin-aware namespacing, an inspection/approval store, update revocation, cache isolation, prompt
injection tests, and a target-client conformance suite. It would also create a new release-coupling
surface between this server and each host's skill loader.

Rollback after a future gated prototype would remove the extension advertisement and `skill://`
resources while leaving all existing tools/resources intact. Clients must always fall back to the
normal MCP contract when the extension is absent.

## Revisit conditions

Re-open this decision only when all of the following are true:

1. SEP-2640, or its successor, is merged and published as an accepted, versioned MCP extension.
2. At least one target client has a public supported release—not a fork or internal prototype—with
   documented extension behavior and a testable compatibility contract.
3. The client implements the proposal's origin, consent, permission, collision, digest, and cache
   safeguards, or stronger equivalents.
4. Artifact MCP has an automated negative-security suite proving that skill content cannot grant
   tools, cross origins, bypass ownership/admin checks, or promote artifact content into privileged
   instructions.
5. Product and security owners explicitly approve a time-boxed `artifact-review` prototype.

Until then, the decision remains defer and the production feature surface remains absent.
