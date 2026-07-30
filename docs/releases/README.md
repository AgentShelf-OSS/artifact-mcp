# Release notes required by the provenance workflow

For every signed release tag, add `docs/releases/vMAJOR.MINOR.PATCH.md` before pushing the tag.
Include:

- the schema version before and after the release;
- migration/operator steps, or `No schema migration.`;
- configuration changes and compatibility impact;
- the immutable backup identity that will be used for promotion; and
- any approved vulnerability exception, including an expiry and remediation owner.

The release workflow refuses to promote a tag without this file. The note is operator evidence and
must not include secrets.
