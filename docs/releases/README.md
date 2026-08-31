# Release notes required by the release workflow

Add `docs/releases/vMAJOR.MINOR.PATCH.md` before pushing an annotated release tag.

Each note must include:

- the schema version before and after the release;
- migration steps, or `No schema migration.`;
- configuration and compatibility changes;
- deployment and rollback guidance that applies to any self-hosted operator; and
- any approved vulnerability exception, including its expiry and remediation owner.

The public note must not contain secrets, private hostnames, production backup paths, customer
data, or environment-specific credentials. Each operator records backup identity, approval, and
rollback evidence in their own deployment system.
