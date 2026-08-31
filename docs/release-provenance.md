# Release provenance

The `Release` GitHub Actions workflow publishes the public Artifact MCP release. It does not
approve or perform a deployment.

## Public release flow

1. The workflow accepts an annotated `vMAJOR.MINOR.PATCH` tag. The tag must match both
   `Cargo.toml` and `package.json`, and its commit must be on the default branch.
2. A successful `CI` push run must already exist for the exact tagged commit.
3. The workflow builds one candidate image, boots every frozen historical fixture with that exact
   digest, generates source and image SBOMs, and rejects high or critical vulnerability findings.
4. GitHub's OIDC-backed attestation service records provenance for the native binary and OCI
   digest. No repository signing key is required.
5. The workflow assigns the stable image tag to the verified digest without rebuilding it, writes
   the release manifest and checksums, and publishes the GitHub Release assets.

The stable GHCR tag is a pointer. The digest recorded in `release-manifest.json` is the immutable
image identity. A retry may reuse an existing stable tag only when it already resolves to the same
candidate digest. If publication fails after the candidate is built, use **Re-run failed jobs** on
that workflow run so the same candidate and attestations are retained.

## Repository inputs

Before creating a tag, add `docs/releases/<tag>.md`. Record schema changes, operator-visible
configuration changes, compatibility notes, and any time-limited vulnerability exception. Do not
include deployment credentials, private hostnames, backup paths, or production data.

Create an annotated tag after the release commit has passed CI:

```sh
git tag -a v1.7.0 -m "Artifact MCP 1.7.0"
git push origin v1.7.0
```

Repository Actions must be allowed to write packages and attestations for the candidate job and
write release contents for the publish job. Protect `v*` tags from deletion and replacement. The
workflow also refuses to point an existing stable image tag at a different digest.

## Release assets

Each GitHub Release contains:

- `artifact-mcp`, copied from the tested final image;
- `source.spdx.json`, the SPDX 2.3 inventory for the tagged source tree;
- `image.spdx.json`, the SPDX 2.3 inventory for the exact candidate digest;
- `historical-fixture-preflight.json`, the exact-image compatibility report;
- `migration-notes.md`, copied from the tracked release note;
- `release-manifest.json`, which binds the tag, commit, image digest, binary, SBOMs, compatibility
  report, and GitHub attestations; and
- `SHA256SUMS`, which covers every downloadable evidence file.

Verify downloaded assets before using them:

```sh
sha256sum --check SHA256SUMS
node scripts/release/verify-manifest.mjs release-manifest.json
gh attestation verify artifact-mcp --repo AgentShelf-OSS/artifact-mcp
gh attestation verify \
  oci://ghcr.io/agentshelf-oss/artifact-mcp@sha256:<digest> \
  --repo AgentShelf-OSS/artifact-mcp
```

## Deployment boundary

A public release cannot prove that an operator has a usable backup or a reachable rollback target.
Those facts exist in the operator's environment, not in this public repository.

Before deploying a release, the operator should:

1. Verify the release checksums, manifest, and attestations.
2. Capture and test a backup of the current database, artifact bodies, previews, and configuration.
3. Record the currently running binary or immutable image digest as the rollback target.
4. Require the approval appropriate for that environment.
5. Install only the checksummed, attested binary or immutable image digest recorded in the release
   manifest.
6. Check `/health`, the migrated schema version, and a representative read and write.

Artifact MCP's homelab deployment procedure belongs in the private homelab operations repository.
It consumes the public release evidence and records backup, approval, deployment, and rollback
details locally.

## Historical fixture evidence

`historical-fixture-preflight.json` exists only after the exact OCI digest boots every immutable
fixture. The report records authenticated reads for every schema boundary and includes write,
update, restore, and durability-recovery checks for the richer released fixtures. The workflow
checksums that report and binds it into `release-manifest.json`.
