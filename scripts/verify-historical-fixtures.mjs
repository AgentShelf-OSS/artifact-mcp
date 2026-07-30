#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
/** Verify the immutable bytes and pre-migration SQLite integrity of every frozen fixture. */
import { createHash } from "node:crypto";
import { readdir, readFile, stat } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import Database from "better-sqlite3";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const FIXTURES = join(ROOT, "conformance", "fixtures", "historical");
const sha256 = (value) => createHash("sha256").update(value).digest("hex");

async function bodyEntries(root) {
  const base = join(root, "artifacts");
  const entries = [];
  async function visit(dir) {
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) await visit(full);
      else if (entry.isFile()) {
        const bytes = await readFile(full);
        entries.push({ path: relative(base, full).replaceAll("\\", "/"), bytes: bytes.length, sha256: sha256(bytes) });
      }
    }
  }
  await visit(base);
  return entries.sort((a, b) => a.path.localeCompare(b.path));
}

const cases = (await readdir(FIXTURES, { withFileTypes: true })).filter((entry) => entry.isDirectory()).map((entry) => entry.name).sort();
if (!cases.length) throw new Error("no historical fixtures found");
for (const name of cases) {
  const root = join(FIXTURES, name);
  const manifest = JSON.parse(await readFile(join(root, "fixture.json"), "utf8"));
  if (manifest.synthetic !== true || manifest.schemaVersion !== 1) throw new Error(`${name}: invalid fixture manifest`);
  const database = await readFile(join(root, manifest.database.path));
  if (sha256(database) !== manifest.database.sha256) throw new Error(`${name}: artifacts.db digest changed`);
  const entries = await bodyEntries(root);
  if (JSON.stringify(entries) !== JSON.stringify(manifest.bodies)) throw new Error(`${name}: body manifest changed`);
  const db = new Database(join(root, manifest.database.path), { readonly: true });
  const integrity = db.pragma("integrity_check", { simple: true });
  db.close();
  if (integrity !== "ok") throw new Error(`${name}: pre-migration integrity_check is ${integrity}`);
  console.log(`${name}: origin schema ${manifest.origin.schemaVersion}, immutable bytes and integrity OK`);
}
