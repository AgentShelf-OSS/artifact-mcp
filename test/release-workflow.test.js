import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("..", import.meta.url);

test("public release workflow stays independent of homelab deployment", async () => {
  const workflow = await readFile(new URL(".github/workflows/release-provenance.yml", root), "utf8");
  const fixtureVerifier = await readFile(new URL("scripts/release/verify-historical-fixtures-in-image.mjs", root), "utf8");

  for (const deploymentGate of [
    "trusted-release-signers",
    "git verify-tag",
    "backup_identity",
    "rollback_image",
    "RELEASE_BACKUP_IDENTITY",
    "RELEASE_ROLLBACK_IMAGE",
  ]) {
    assert.equal(workflow.includes(deploymentGate), false, `${deploymentGate} must not block an OSS release`);
  }
  assert.doesNotMatch(workflow, /environment:\s*production/);

  assert.match(workflow, /Release tags must be annotated/);
  assert.match(workflow, /package\.json version/);
  assert.match(workflow, /Require successful CI for tagged commit/);
  assert.match(workflow, /actions\/workflows\/ci\.yml\/runs\?head_sha=/);
  assert.match(workflow, /status=completed/);
  assert.doesNotMatch(workflow, /status=success/);
  assert.match(workflow, /git merge-base --is-ancestor/);
  assert.match(workflow, /docker\/build-push-action/);
  assert.match(workflow, /--env AUDIT_LEDGER_HMAC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=/);
  assert.match(workflow, /--tmpfs \/data-rust:rw,nosuid,nodev,noexec,size=64m,mode=1777,uid=65532,gid=65532/);
  assert.doesNotMatch(workflow, /docker run --detach --rm --name artifact-mcp-release-candidate/);
  assert.match(workflow, /docker rm --force artifact-mcp-release-candidate/);
  assert.match(workflow, /verify-historical-fixtures-in-image\.mjs/);
  assert.match(fixtureVerifier, /AUDIT_LEDGER_HMAC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=/);
  assert.doesNotMatch(fixtureVerifier, /"--detach", "--rm"/);
  assert.match(fixtureVerifier, /\["rm", "--force", container\]/);
  assert.match(workflow, /anchore\/sbom-action/);
  assert.match(workflow, /anchore\/scan-action/);
  assert.equal(workflow.match(/actions\/attest-build-provenance/g)?.length, 2);
  assert.match(workflow, /sha256sum --check SHA256SUMS/);
  assert.match(workflow, /tag_name: \$\{\{ needs\.prepare\.outputs\.tag \}\}/);
  assert.match(workflow, /prerelease: \$\{\{ contains\(needs\.prepare\.outputs\.tag, '-'\) \}\}/);
  assert.match(workflow, /make_latest: \$\{\{ !contains\(needs\.prepare\.outputs\.tag, '-'\) \}\}/);
  assert.match(workflow, /Existing release image digest .* does not match candidate/);
  for (const asset of [
    "artifact-mcp",
    "SHA256SUMS",
    "release-manifest.json",
    "source.spdx.json",
    "image.spdx.json",
    "historical-fixture-preflight.json",
    "migration-notes.md",
  ]) {
    assert.match(workflow, new RegExp(`release/${asset.replace(".", "\\.")}`));
  }
});

test("release documentation keeps deployment evidence local", async () => {
  const documentation = await readFile(new URL("docs/release-provenance.md", root), "utf8");
  assert.match(documentation, /Deployment boundary/);
  assert.match(documentation, /operator's environment/);
  assert.match(documentation, /Re-run failed jobs/);
  assert.doesNotMatch(documentation, /trusted-release-signers|RELEASE_BACKUP_IDENTITY|RELEASE_ROLLBACK_IMAGE/);
});
