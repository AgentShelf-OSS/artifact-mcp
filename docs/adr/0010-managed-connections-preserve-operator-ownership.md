# Managed connections preserve operator ownership

- Status: Accepted
- Issue: [#59](https://github.com/AgentShelf-OSS/artifact-mcp/issues/59)

Administrator-managed connections persist in SQLite alongside artifact bindings. Sources from the
operator file retain separate ownership and appear read-only in administration. IDs must be unique
across both origins; a collision fails startup instead of overriding either definition. This keeps
a deployment change from silently replacing an administrator's API grant or making an operator
connection editable.

Source impact checks, configuration writes, and audit events share one writer transaction. Runtime
changes publish only after commit. Changed topics reconnect and resynchronize independently, so a
connection edit can preserve other sources in the same viewer stream. Credential values remain in
protected deployment configuration; managed definitions store environment reference names only.
