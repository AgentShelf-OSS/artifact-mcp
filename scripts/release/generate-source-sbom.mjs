#!/usr/bin/env node
/**
 * Generate a deterministic SPDX 2.3 source SBOM from the files committed at HEAD.
 *
 * This intentionally has no package-manager dependency: a release workflow must be able to
 * describe the exact checked-out source before it trusts or installs application dependencies.
 */
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";

function usage(message) {
  if (message) process.stderr.write(`error: ${message}\n`);
  process.stderr.write(
    "usage: generate-source-sbom.mjs --out <path> --commit <40-hex> --version <version> --created <UTC ISO-8601> --repository <owner/repo>\n",
  );
  process.exit(message ? 2 : 0);
}

function args(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index];
    const value = argv[index + 1];
    if (!key?.startsWith("--") || value === undefined || values.has(key)) usage("invalid arguments");
    values.set(key, value);
  }
  const allowed = new Set(["--out", "--commit", "--version", "--created", "--repository"]);
  for (const key of values.keys()) if (!allowed.has(key)) usage(`unknown option ${key}`);
  for (const key of allowed) if (!values.has(key)) usage(`missing ${key}`);
  return Object.fromEntries(values.entries());
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function requireMatch(value, expression, label) {
  if (!expression.test(value)) usage(`${label} has an invalid format`);
  return value;
}

const options = args(process.argv.slice(2));
const commit = requireMatch(options["--commit"], /^[a-f0-9]{40}$/, "commit");
const version = requireMatch(options["--version"], /^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$/, "version");
const created = requireMatch(options["--created"], /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/, "created");
const repository = requireMatch(options["--repository"], /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/, "repository");
const out = resolve(options["--out"]);
const checkedOutCommit = execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim();
if (checkedOutCommit !== commit) usage("commit does not match checked-out source");

const files = execFileSync("git", ["ls-files", "-z"], { encoding: "buffer" })
  .toString("utf8")
  .split("\0")
  .filter(Boolean)
  .sort();

const packageId = "SPDXRef-Package-artifact-mcp";
const document = {
  SPDXID: "SPDXRef-DOCUMENT",
  spdxVersion: "SPDX-2.3",
  name: `artifact-mcp-source-${version}`,
  dataLicense: "CC0-1.0",
  documentNamespace: `https://github.com/${repository}/releases/source/${commit}`,
  creationInfo: {
    creators: ["Tool: artifact-mcp-release-source-sbom"],
    created,
  },
  packages: [
    {
      SPDXID: packageId,
      name: "artifact-mcp",
      versionInfo: version,
      downloadLocation: "NOASSERTION",
      filesAnalyzed: true,
      licenseConcluded: "Apache-2.0",
      licenseDeclared: "Apache-2.0",
      copyrightText: "NOASSERTION",
      externalRefs: [
        {
          referenceCategory: "OTHER",
          referenceType: "git",
          referenceLocator: `git+https://github.com/${repository}@${commit}`,
        },
      ],
    },
  ],
  files: [],
  relationships: [{ spdxElementId: "SPDXRef-DOCUMENT", relationshipType: "DESCRIBES", relatedSpdxElement: packageId }],
};

for (const [index, file] of files.entries()) {
  const contents = await readFile(resolve(file));
  const SPDXID = `SPDXRef-File-${index + 1}`;
  document.files.push({
    SPDXID,
    fileName: `./${file}`,
    checksums: [{ algorithm: "SHA256", checksumValue: sha256(contents) }],
    licenseConcluded: "NOASSERTION",
    copyrightText: "NOASSERTION",
  });
  document.relationships.push({ spdxElementId: packageId, relationshipType: "CONTAINS", relatedSpdxElement: SPDXID });
}

await mkdir(dirname(out), { recursive: true });
await writeFile(out, `${JSON.stringify(document, null, 2)}\n`);
