#!/usr/bin/env node
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";

const DIGEST = /^sha256:[a-f0-9]{64}$/;
const REPOSITORY = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const RELEASE_TAG = /^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;
const RELEASE_VERSION = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;
const GHCR_IMAGE_REFERENCE = /^ghcr\.io\/([a-z0-9][a-z0-9._-]*)\/([a-z0-9][a-z0-9._-]*):([A-Za-z0-9_][A-Za-z0-9_.-]{0,127})$/;

function fail(message) {
  process.stderr.write(`release manifest verification failed: ${message}\n`);
  process.exit(1);
}

function isSafeArtifactPath(value) {
  return typeof value === "string"
    && /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(value);
}

function imageRepository(reference) {
  const match = GHCR_IMAGE_REFERENCE.exec(reference);
  if (!match) fail("invalid GHCR image reference");
  return { repositoryPart: `ghcr.io/${match[1]}/${match[2]}`, githubRepository: `${match[1]}/${match[2]}` };
}

async function verifyHashedArtifact(manifestDirectory, item, label) {
  if (!isSafeArtifactPath(item?.path) || !/^[a-f0-9]{64}$/.test(item.sha256 ?? "")) {
    fail(`invalid ${label} digest record`);
  }
  try {
    const actual = createHash("sha256")
      .update(await readFile(resolve(manifestDirectory, item.path)))
      .digest("hex");
    if (actual !== item.sha256) fail(`digest mismatch for ${label}`);
  } catch (error) {
    fail(`cannot hash ${label}: ${error.message}`);
  }
}

if (process.argv.length !== 3) fail("usage: verify-manifest.mjs <release-manifest.json>");
const manifestPath = resolve(process.argv[2]);
const manifestDirectory = dirname(manifestPath);
let manifest;
try {
  manifest = JSON.parse(await readFile(manifestPath, "utf8"));
} catch (error) {
  fail(`cannot read JSON: ${error.message}`);
}

if (manifest?.schemaVersion !== 1) fail("unsupported schemaVersion");
const release = manifest.release;
if (!RELEASE_TAG.test(release?.tag ?? "")) fail("invalid tag");
if (!RELEASE_VERSION.test(release.version ?? "") || release.tag.slice(1) !== release.version) {
  fail("tag/version mismatch");
}
if (!/^[a-f0-9]{40}$/.test(release.commit ?? "")) fail("invalid commit");
if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(release.created ?? "")
  || Number.isNaN(Date.parse(release.created))
  || new Date(release.created).toISOString().replace(".000Z", "Z") !== release.created) {
  fail("invalid created timestamp");
}

const image = manifest.image;
const { repositoryPart, githubRepository } = imageRepository(image?.reference ?? "");
if (!DIGEST.test(image.digest ?? "")) fail("invalid image digest");
if (!image.reference.endsWith(`:${release.tag}`)) fail("image tag does not match release tag");
const immutableReference = `${repositoryPart}@${image.digest}`;
if (image.immutableReference !== immutableReference) fail("immutable image reference does not match image identity");

if (!Number.isSafeInteger(manifest.schema?.minCompatible)
  || !Number.isSafeInteger(manifest.schema?.current)
  || !Number.isSafeInteger(manifest.schema?.maxCompatible)
  || manifest.schema.minCompatible < 0
  || manifest.schema.minCompatible > manifest.schema.current
  || manifest.schema.current > manifest.schema.maxCompatible) {
  fail("invalid schema range");
}

await verifyHashedArtifact(manifestDirectory, manifest.binary, "binary");
if (!Array.isArray(manifest.sboms) || manifest.sboms.length !== 2) fail("missing SBOMs");
const expectedSbomKinds = ["source", "image"];
for (const [index, item] of manifest.sboms.entries()) {
  if (item?.kind !== expectedSbomKinds[index] || item.format !== "SPDX-2.3") fail("invalid SBOM record");
  await verifyHashedArtifact(manifestDirectory, item, `${item.kind} SBOM`);
}

const recovery = manifest.recovery;
if (typeof recovery?.backupIdentity !== "string" || !/\S/.test(recovery.backupIdentity)
  || recovery.backupIdentity.length > 512 || /[\r\n\0]/.test(recovery.backupIdentity)) {
  fail("invalid backup identity");
}
if (!new RegExp(`^${repositoryPart.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}@sha256:[a-f0-9]{64}$`).test(recovery.rollbackImage ?? "")) {
  fail("invalid rollback image");
}
await verifyHashedArtifact(manifestDirectory, recovery.migrationNotes, "migration notes");
if (recovery.historicalFixturePreflight !== "exact-oci-historical-fixtures/v1") fail("unexpected fixture preflight state");
await verifyHashedArtifact(manifestDirectory, recovery.historicalFixturePreflightReport, "historical fixture preflight report");
let fixtureReport;
try {
  fixtureReport = JSON.parse(await readFile(resolve(manifestDirectory, recovery.historicalFixturePreflightReport.path), "utf8"));
} catch (error) {
  fail(`cannot parse historical fixture preflight report: ${error.message}`);
}
if (fixtureReport?.schemaVersion !== 1 || fixtureReport?.error) fail("historical fixture preflight did not complete cleanly");
if (fixtureReport.image !== immutableReference) fail("historical fixture preflight image does not match immutable release image");
if (!Array.isArray(fixtureReport.fixtures) || fixtureReport.fixtures.length === 0
  || fixtureReport.fixtures.some((fixture) => fixture?.status !== "passed")) {
  fail("historical fixture preflight has missing or failed cases");
}

if (!Array.isArray(manifest.attestations) || manifest.attestations.length !== 2) fail("missing provenance attestations");
const [binaryAttestation, imageAttestation] = manifest.attestations;
const repository = binaryAttestation?.repository;
if (!REPOSITORY.test(repository ?? "") || imageAttestation?.repository !== repository) fail("invalid attestation repository");
if (repository.toLowerCase() !== githubRepository) fail("attestation repository does not match image repository");
const expectedAttestations = [
  {
    value: binaryAttestation,
    kind: "github-build-provenance",
    subject: manifest.binary.path,
    verification: `gh attestation verify ${manifest.binary.path} --repo ${repository}`,
  },
  {
    value: imageAttestation,
    kind: "oci-build-provenance",
    subject: immutableReference,
    verification: `gh attestation verify oci://${immutableReference} --repo ${repository}`,
  },
];
for (const expected of expectedAttestations) {
  const value = expected.value;
  if (value?.kind !== expected.kind || value.subject !== expected.subject || value.verification !== expected.verification) {
    fail("attestation does not bind to release identity");
  }
  if (!new RegExp(`^https://github\\.com/${repository.replace("/", "\\/")}/attestations/[0-9]+$`).test(value.reference ?? "")) {
    fail("invalid attestation reference");
  }
}

process.stdout.write(`verified ${release.tag} (${image.digest})\n`);
