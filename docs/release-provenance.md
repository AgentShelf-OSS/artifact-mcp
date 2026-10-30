# Release provenance

The `Release` GitHub Actions workflow publishes the public Artifact MCP release. It does not
approve or perform a deployment.

## Public release flow

1. The workflow accepts an annotated `vMAJOR.MINOR.PATCH` tag. The tag must match the `Cargo.toml`
   version, and its commit must be on the default branch.
2. A successful `CI` push run must already exist for the exact tagged commit.
3. The `build` job builds one candidate image and pushes it to a registry on the runner's
   loopback. It boots every frozen historical fixture with that exact digest, generates source and
   image SBOMs, and rejects high or critical vulnerability findings. From the same checkout it
   builds the preview renderer, `artifact-preview`, as a static musl binary and refuses it unless
   `file` reports `static-pie linked` or `statically linked`. It exports the candidate index, with
   every digest unchanged, as an OCI archive.
4. The `attest` job checks the build job's checksums and that the archive holds the tested digest.
   It then pushes that index to GHCR with its digest unchanged. GitHub's OIDC-backed attestation
   service records provenance for the native server binary, the preview renderer binary, and the
   OCI digest. No repository signing key is required.
5. The `manifest` job writes and verifies the release manifest and the final checksums.
6. The `publish` job verifies the checksums and all three attestations. It assigns the stable image
   tag to the verified digest without rebuilding it, attaches every asset to a draft GitHub
   Release, and then publishes the complete draft.

### Job isolation

A release compiles and runs code from every locked dependency: build scripts, procedural macros,
and the `xtask` tool. Only the `build` and `manifest` jobs do that, and they hold only
`contents: read`, no registry credential, and no checkout token. The `attest` and `publish` jobs
hold the write permissions. They never check out, compile, or run repository code; they check and
move the bytes the other jobs produced, with GitHub actions, `skopeo`, `docker`, and `gh`. A
compromised dependency can still change the binary it is compiled into. It cannot mint
attestations for other subjects, push images, or change a release.
`xtask/tests/release_workflow.rs` enforces this split.

Files that pass between jobs are workflow artifacts, which code in the `build` job could try to
replace. The `attest` job therefore records a hash of the candidate checksums as a job output.
Before it promotes anything, `publish` checks the downloaded files against that hash and checks
that `release-manifest.json` names the attested image digest and server binary. The fixture
preflight boots the image from the loopback registry. The build job then checks that the report
names that digest and replaces its `image` with the GHCR reference for the same digest, which
`release verify-manifest` requires. The workflow uses no xtask option newer than the tag it
releases, so it can still run against older tags.

To rehearse the pipeline without publishing, run the workflow by hand with an existing tag and
`rehearse: true`. The rehearsal pushes and attests under `ghcr.io/<owner>/artifact-mcp-rehearsal`
and skips the `publish` job. `release verify-manifest` requires the release image name, so a
rehearsal checks only that the manifest and the fixture preflight name the same image. Its
attestations name the branch that ran it, not a release tag, so a verifier that pins the tag
rejects them.

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
git tag -a v1.7.2 -m "Artifact MCP 1.7.2"
git push origin v1.7.2
```

Repository Actions must be allowed to write packages and attestations for the `attest` job and
write release contents for the `publish` job. Protect `v*` tags from deletion and replacement. The
workflow also refuses to point an existing stable image tag at a different digest. Enable GitHub
release immutability for the repository. GitHub applies that policy to future releases only, so the
workflow uploads every asset to a draft before publishing and locking it.

## Release assets

Each GitHub Release contains:

- `artifact-mcp`, copied from the tested final image;
- `artifact-preview`, the static preview renderer binary (`preview-renderer/`), built from the
  tagged source with
  `cargo build --release --locked --target x86_64-unknown-linux-musl --manifest-path preview-renderer/Cargo.toml`;
- `source.spdx.json`, the SPDX 2.3 inventory for the tagged source tree;
- `image.spdx.json`, the SPDX 2.3 inventory for the exact candidate digest. The build job scans
  the digest on its loopback registry, so the SBOM names the source `localhost:5000/artifact-mcp`;
- `historical-fixture-preflight.json`, the exact-image compatibility report;
- `migration-notes.md`, copied from the tracked release note;
- `release-manifest.json`, which binds the tag, commit, image digest, binary, SBOMs, compatibility
  report, and GitHub attestations; and
- `SHA256SUMS`, which covers every downloadable evidence file.

`release-manifest.json` does not name `artifact-preview`, and its format did not change when the
renderer was added. The renderer binary is bound by its `SHA256SUMS` entry and by its own build
provenance attestation. The image SBOM and the vulnerability scan cover the server image, not the
renderer binary. The renderer's dependencies are locked in `preview-renderer/Cargo.lock`.

Verify downloaded assets before using them. Run `cargo xtask` from a checkout of the release tag;
the manifest path can be absolute or relative to the current directory:

```sh
(cd <download-dir> && sha256sum --check SHA256SUMS)
cargo --locked xtask release verify-manifest <download-dir>/release-manifest.json
gh attestation verify <download-dir>/artifact-mcp --repo AgentShelf-OSS/artifact-mcp
gh attestation verify <download-dir>/artifact-preview --repo AgentShelf-OSS/artifact-mcp
gh attestation verify \
  oci://ghcr.io/agentshelf-oss/artifact-mcp@sha256:<digest> \
  --repo AgentShelf-OSS/artifact-mcp
```

To install only the renderer, download it with `SHA256SUMS` and check both. Pinning the signing
workflow, the tag and its commit is stricter than `--repo` alone. `sha256sum` fails with
`no file was verified` when `SHA256SUMS` has no `artifact-preview` line.

```sh
TAG=vX.Y.Z
REPO=AgentShelf-OSS/artifact-mcp
COMMIT="$(gh api "repos/$REPO/commits/$TAG" --jq .sha)"
gh release download "$TAG" --repo "$REPO" -p artifact-preview -p SHA256SUMS
sha256sum --check --strict --ignore-missing SHA256SUMS
gh attestation verify artifact-preview --repo "$REPO" \
  --cert-identity "https://github.com/$REPO/.github/workflows/release-provenance.yml@refs/tags/$TAG" \
  --cert-oidc-issuer https://token.actions.githubusercontent.com \
  --source-ref "refs/tags/$TAG" --source-digest "$COMMIT"
```

`preview-renderer/DEPLOY-CT222.md` uses these commands for a native renderer host.

`verify-manifest` exits 0 and prints `verified <tag> (<digest>)` when the manifest and every file
it names are consistent. It exits 1 and prints `release manifest verification failed: <reason>`
otherwise.

## Release tooling

The workflow runs these `cargo xtask release` commands. Each job builds the tool with
`cargo build --locked --package xtask` before its first registry or release side effect.

| Command | Purpose |
| --- | --- |
| `source-sbom --out PATH --commit SHA --version X.Y.Z --created UTC --repository OWNER/REPO` | Write the SPDX 2.3 inventory of the files committed at `HEAD`. It refuses a different commit, or a tracked file that differs from `HEAD`. |
| `verify-fixtures-in-image IMAGE@sha256:DIGEST OUT.json` | Copy every frozen historical fixture, boot each copy with the exact image through Docker, and write the preflight report. |
| `manifest --out PATH --tag TAG ...` | Write `release-manifest.json` from arguments only. Run `cargo xtask release manifest --help` for the flags. |
| `verify-manifest FILE` | Check the manifest, the files beside it, and their bindings to the tag, commit, image digest, and attestations. |

`verify-fixtures-in-image --binary PATH [--fixtures DIR] OUT.json` runs the same fixture checks
against a native `artifact-mcp` binary, for a host without a Docker daemon. Its report records
`local-binary:<path>@sha256:<digest>` as the image, so `verify-manifest` never accepts it as
release evidence.

Deployment scripts that ran `node scripts/release/verify-manifest.mjs <manifest>` must run
`cargo --locked xtask release verify-manifest <manifest>` from a checkout of the release tag
instead. A host without a Rust toolchain can use a prebuilt tool: run
`cargo build --locked --release --package xtask` on a build host, copy `target/release/xtask`,
and run `xtask release verify-manifest <manifest>`. The argument, the failure messages, and the
exit codes (0 verified, 1 rejected) are the same as those of the Node script.

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
