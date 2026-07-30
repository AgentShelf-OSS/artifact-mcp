#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
// Invoked in a fresh process per copied fixture so lib/db.js observes that fixture's DATA_DIR.
import db, { ARTIFACT_DIR } from "../lib/db.js";
import { auditStorage, backfillBodyDigests } from "../lib/store.js";
import * as webhooks from "../lib/webhooks.js";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

const report = auditStorage({ cleanTransient: true });
const backfill = backfillBodyDigests();
const encrypted = webhooks.forEvent("fixture", "updated").map((row) => row.url);
if (encrypted.length && !encrypted.some((url) => url.includes("synthetic-encrypted-token"))) {
  throw new Error("encrypted fixture webhook was not decrypted by the Node runtime");
}
const integrity = db.pragma("integrity_check", { simple: true });
const remainingIntentIds = db.prepare("SELECT id FROM artifact_durability_intents ORDER BY id").pluck().all();
const prepared = db.prepare("SELECT id, revision FROM artifacts WHERE id = 'prepared24'").get();
const preparedHistoryPath = join(ARTIFACT_DIR, ".history", "prepared24", "1.html");
const preparedMetadataOnly = prepared ? {
  id: prepared.id,
  revision: prepared.revision,
  historyRevision: 1,
  html: existsSync(preparedHistoryPath) ? readFileSync(preparedHistoryPath, "utf8") : null,
} : null;
db.close();
if (integrity !== "ok") throw new Error(`post-migration integrity_check is ${integrity}`);
process.stdout.write(`${JSON.stringify({ report, backfill, encrypted: encrypted.length, remainingIntentIds, preparedMetadataOnly })}\n`);
