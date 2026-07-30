# Release provenance

PBI-047 makes a release a chain of immutable evidence rather than a rebuild performed during
deployment. The [`release-provenance` workflow](../.github/workflows/release-provenance.yml) has
two stages:

1. It verifies an annotated, GPG-signed `vMAJOR.MINOR.PATCH` tag; creates one candidate image;
   tests, scans, generates SBOMs for, and attests that digest; and saves the exact native binary.
2. The protected `production` environment promotes that tested digest to the release tag without a
   rebuild, verifies the digest again, writes `release-manifest.json` and `SHA256SUMS`, and attaches
   all evidence to a GitHub Release.

The candidate tag is allowed to exist if a scan fails, but it is not a production tag. A failed
promotion cannot replace an existing stable tag or GitHub Release. Deployments must consume the
manifest's `image.immutableReference` (`repository@sha256:…`), never a mutable tag.

## Repository inputs

Before enabling this workflow, commit a reviewable ASCII-armored keyring at
`ops/release/trusted-release-signers.asc`. It must contain only the public keys allowed to sign
release tags. The workflow creates an empty temporary keyring and runs `git verify-tag`, so a
GitHub “Verified” badge alone is insufficient.

Every release tag also needs a tracked `docs/releases/<tag>.md` file. It records configuration
changes and migration operator notes. Keep a release note even when there is no schema change;
write `No schema migration.` explicitly.

Release tags support a SemVer prerelease suffix (for example `v1.7.0-rc.1`) but intentionally do
not support SemVer build metadata (`+build.5`), because `+` is not valid in an OCI tag. The tag and
the Cargo package version must otherwise match exactly.

## Required GitHub configuration

These controls cannot be created safely from repository code and remain an operator task:

- Create a protected **production** environment with an explicit approval rule. The promotion job
  uses it before the stable GHCR tag or GitHub Release is written.
- Allow the workflow `GITHUB_TOKEN` to write GitHub Packages and attestations, or replace the token
  with a narrowly-scoped package token.
- Set repository/environment variables `RELEASE_BACKUP_IDENTITY` and
  `RELEASE_ROLLBACK_IMAGE`. The latter must be a previously verified
  `repository@sha256:<64-hex>` reference. A manual dispatch can supply one-release overrides.
- Protect release tags, disallow force-pushes, and enable immutable GitHub Releases/GHCR tags where
  the organization plan supports them. The workflow refuses an already-existing release or image
  tag, but registry-side immutability is the final control.
- Make the deployment system read `release-manifest.json`, verify its SHA256 set and attestations,
  take the recorded backup, and then deploy only its immutable image reference. Do not pass secrets
  as Docker build arguments; use runtime secrets or BuildKit secret mounts.

## Evidence and verification

Release assets are:

- `artifact-mcp` — binary copied from the tested final image;
- `source.spdx.json` — deterministic SPDX 2.3 inventory of the exact Git source tree;
- `image.spdx.json` — SPDX 2.3 SBOM generated from the final candidate image digest;
- `SHA256SUMS` — hashes for every downloadable evidence artifact; and
- `release-manifest.json` — tag/commit, binary and image identity, schema range, SBOM paths,
  direct attestation references and verification commands, backup identity, migration notes, and
  rollback image; and
- `migration-notes.md` — the exact tracked `docs/releases/<tag>.md` file copied into the release
  evidence set and checksum list.

After downloading release assets, verify them before deployment:

```sh
sha256sum --check SHA256SUMS
node scripts/release/verify-manifest.mjs release-manifest.json
gh attestation verify artifact-mcp --repo AgentShelf-OSS/artifact-mcp
gh attestation verify oci://ghcr.io/agentshelf-oss/artifact-mcp@sha256:<digest> \
  --repo AgentShelf-OSS/artifact-mcp
```

The workflow rejects high and critical vulnerability findings. There are deliberately no silent
exceptions; if an exception becomes necessary, document its CVE, affected digest, expiry, and
approval in the release note and use a separately reviewed scanner configuration change.

## Historical-fixture boundary

`release-manifest.json` records `historicalFixturePreflight: "exact-oci-historical-fixtures/v1"` only after the
release workflow has run every immutable fixture through the **exact OCI digest**. The attached,
checksummed `historical-fixture-preflight.json` records the image identity and every boot/read
result; the rich recovery fixture additionally proves authenticated write, update, and restore.
