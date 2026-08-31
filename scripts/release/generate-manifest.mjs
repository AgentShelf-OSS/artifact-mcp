#!/usr/bin/env node
/**
 * Construct the release provenance document. The argument-only interface makes the artifact
 * reproducible and prevents ambient CI variables from becoming unrecorded release inputs.
 */
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";

const REQUIRED = [
  "--out", "--tag", "--version", "--commit", "--created", "--binary", "--image-ref", "--image-digest",
  "--schema-min", "--schema-current", "--schema-max", "--source-sbom", "--image-sbom",
  "--migration-notes", "--fixture-preflight", "--repository", "--binary-attestation", "--image-attestation",
];

function usage(message) {
  if (message) process.stderr.write(`error: ${message}\n`);
  process.stderr.write(`usage: generate-manifest.mjs ${REQUIRED.map((key) => `${key} <value>`).join(" ")}\n`);
  process.exit(message ? 2 : 0);
}

function parse(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index];
    const value = argv[index + 1];
    if (!key?.startsWith("--") || value === undefined || values.has(key)) usage("invalid arguments");
    values.set(key, value);
  }
  for (const key of values.keys()) if (!REQUIRED.includes(key)) usage(`unknown option ${key}`);
  for (const key of REQUIRED) if (!values.has(key)) usage(`missing ${key}`);
  return Object.fromEntries(values.entries());
}

function valid(value, expression, label) {
  if (!expression.test(value)) usage(`${label} has an invalid format`);
  return value;
}

async function digest(path) {
  return createHash("sha256").update(await readFile(resolve(path))).digest("hex");
}

const input = parse(process.argv.slice(2));
const tag = valid(input["--tag"], /^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$/, "tag");
const version = valid(input["--version"], /^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$/, "version");
if (tag.slice(1) !== version) usage("tag must be v followed by version");
const commit = valid(input["--commit"], /^[a-f0-9]{40}$/, "commit");
const created = valid(input["--created"], /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/, "created");
const imageDigest = valid(input["--image-digest"], /^sha256:[a-f0-9]{64}$/, "image digest");
const schema = ["--schema-min", "--schema-current", "--schema-max"].map((key) => {
  const parsed = Number(input[key]);
  if (!Number.isSafeInteger(parsed) || parsed < 0) usage(`${key} must be a non-negative integer`);
  return parsed;
});
if (schema[0] > schema[1] || schema[1] > schema[2]) usage("schema range must satisfy min <= current <= max");
const repository = valid(input["--repository"], /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/, "repository");
const binaryAttestation = valid(input["--binary-attestation"], /^https:\/\/github\.com\/.+$/, "binary attestation");
const imageAttestation = valid(input["--image-attestation"], /^https:\/\/github\.com\/.+$/, "image attestation");

const binary = resolve(input["--binary"]);
const sourceSbom = resolve(input["--source-sbom"]);
const imageSbom = resolve(input["--image-sbom"]);
const migrationNotes = resolve(input["--migration-notes"]);
const fixturePreflight = resolve(input["--fixture-preflight"]);
const manifest = {
  schemaVersion: 2,
  release: { tag, version, commit, created },
  image: {
    reference: input["--image-ref"],
    digest: imageDigest,
    immutableReference: `${input["--image-ref"].replace(/:[^/:]+$/, "")}@${imageDigest}`,
  },
  binary: { path: input["--binary"], sha256: await digest(binary) },
  schema: { minCompatible: schema[0], current: schema[1], maxCompatible: schema[2] },
  sboms: [
    { kind: "source", path: input["--source-sbom"], sha256: await digest(sourceSbom), format: "SPDX-2.3" },
    { kind: "image", path: input["--image-sbom"], sha256: await digest(imageSbom), format: "SPDX-2.3" },
  ],
  attestations: [
    {
      kind: "github-build-provenance",
      repository,
      subject: input["--binary"],
      reference: binaryAttestation,
      verification: `gh attestation verify ${input["--binary"]} --repo ${repository}`,
    },
    {
      kind: "oci-build-provenance",
      repository,
      subject: `${input["--image-ref"].replace(/:[^/:]+$/, "")}@${imageDigest}`,
      reference: imageAttestation,
      verification: `gh attestation verify oci://${input["--image-ref"].replace(/:[^/:]+$/, "")}@${imageDigest} --repo ${repository}`,
    },
  ],
  validation: {
    migrationNotes: { path: input["--migration-notes"], sha256: await digest(migrationNotes) },
    // The release workflow runs scripts/release/verify-historical-fixtures-in-image.mjs against
    // the exact OCI digest before this manifest is generated; its JSON report is checksummed and
    // attached beside this manifest.
    historicalFixturePreflight: "exact-oci-historical-fixtures/v1",
    historicalFixturePreflightReport: { path: input["--fixture-preflight"], sha256: await digest(fixturePreflight) },
  },
};

const out = resolve(input["--out"]);
await mkdir(dirname(out), { recursive: true });
await writeFile(out, `${JSON.stringify(manifest, null, 2)}\n`);
