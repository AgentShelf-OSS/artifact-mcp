import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import Database from "better-sqlite3";

const script = process.env.ARTIFACT_MCP_BACKUP_SCRIPT || path.resolve("scripts/backup.sh");
const sqlite = spawnSync("which", ["sqlite3"], { encoding: "utf8" }).stdout.trim();
const quoteShell = (value) => "'" + value.replaceAll("'", "'\"'\"'") + "'";
const hash = (value) => createHash("sha256").update(value).digest("hex");

async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "artifact-backup-"));
  const data = path.join(root, "data");
  const backups = path.join(root, "backups");
  await fs.mkdir(path.join(data, "artifacts"), { recursive: true });
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const bin = path.join(root, "bin");
  await fs.mkdir(bin);
  await fs.writeFile(path.join(bin, "sqlite3"), `#!/usr/bin/env bash
set -euo pipefail
pause() {
  : > "$BACKUP_TEST_PAUSE"
  while [ -e "$BACKUP_TEST_PAUSE" ]; do sleep 0.01; done
}
if [[ "\${2:-}" == VACUUM* && -n "\${BACKUP_TEST_PAUSE:-}" ]]; then
  if [[ "$BACKUP_TEST_PHASE" == before ]]; then pause; fi
  ${quoteShell(sqlite)} "$@"
  if [[ "$BACKUP_TEST_PHASE" == after ]]; then pause; fi
else
  exec ${quoteShell(sqlite)} "$@"
fi
`, { mode: 0o755 });
  await fs.writeFile(path.join(bin, "cp"), `#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == "-a" && "\${2:-}" == */previews && -n "\${BACKUP_TEST_REMOVE_PREVIEWS:-}" ]]; then
  rm -rf "\${2}"
  exit 0
fi
/usr/bin/cp "$@"
if [[ "\${1:-}" == "-a" && "\${2:-}" == */previews && -n "\${BACKUP_TEST_CORRUPT_PREVIEW:-}" ]]; then
  printf 'corrupted-preview' > "\${3}/bundle1.png"
fi
`, { mode: 0o755 });
  const db = new Database(path.join(data, "artifacts.db"));
  t.after(() => { if (db.open) db.close(); });
  db.exec("CREATE TABLE artifacts (id TEXT PRIMARY KEY, is_bundle INTEGER NOT NULL, body_sha256 TEXT NOT NULL); CREATE TABLE artifact_revisions (artifact_id TEXT NOT NULL, revision INTEGER NOT NULL, is_bundle INTEGER NOT NULL, body_sha256 TEXT NOT NULL); CREATE TABLE artifact_durability_intents (id TEXT PRIMARY KEY)");
  return { root, data, backups, db };
}

function startBackup(data, backups, pause, phase = "after", keep = "14", extraEnv = {}) {
  const child = spawn("bash", [script, data, backups, keep], {
    env: { ...process.env, PATH: `${path.join(path.dirname(data), "bin")}:${process.env.PATH}`,
      ...(pause ? { BACKUP_TEST_PAUSE: pause, BACKUP_TEST_PHASE: phase } : {}), ...extraEnv },
  });
  const output = [];
  child.stdout.on("data", (chunk) => output.push(chunk));
  child.stderr.on("data", (chunk) => output.push(chunk));
  const timer = setTimeout(() => child.kill("SIGKILL"), 10000);
  return new Promise((resolve, reject) => {
    child.on("error", (error) => { clearTimeout(timer); reject(error); });
    child.on("close", (code) => { clearTimeout(timer); resolve(code); });
  });
}

async function waitForFile(file) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    try { await fs.access(file); return; } catch { await new Promise((resolve) => setTimeout(resolve, 10)); }
  }
  assert.fail("backup did not reach the SQLite barrier within five seconds");
}

async function runBackup(data, backups, keep) { return startBackup(data, backups, undefined, undefined, keep); }

async function release(pause) { await fs.rm(pause); }

test("active publish between file copy and SQLite snapshot cannot yield a missing body", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  const before = Buffer.from("before");
  db.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("before", hash(before));
  db.prepare("INSERT INTO artifact_revisions VALUES (?, 1, 0, ?)").run("before", hash(Buffer.from("previous")));
  await fs.writeFile(path.join(data, "artifacts", "before.html"), before);
  await fs.mkdir(path.join(data, "artifacts", ".history", "before"), { recursive: true });
  await fs.writeFile(path.join(data, "artifacts", ".history", "before", "1.html"), "previous");
  db.close();

  const pause = path.join(root, "pause");
  const running = startBackup(data, backups, pause, "before");
  await waitForFile(pause);
  const during = new Database(path.join(data, "artifacts.db"));
  const body = Buffer.from("during");
  during.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("during", hash(body));
  during.close();
  await fs.writeFile(path.join(data, "artifacts", "during.html"), body);
  await release(pause);
  assert.equal(await running, 0);

  const [name] = await fs.readdir(backups);
  const backupDb = new Database(path.join(backups, name, "artifacts.db"), { readonly: true });
  assert.deepEqual(backupDb.prepare("SELECT id FROM artifacts ORDER BY id").all(), [{ id: "before" }, { id: "during" }]);
  assert.equal(await fs.readFile(path.join(backups, name, "artifacts", "during.html"), "utf8"), "during");
  backupDb.close();
  assert.equal(await fs.readFile(path.join(backups, name, "artifacts", ".history", "before", "1.html"), "utf8"), "previous");
});

test("canonical bundle and retained bundle history are verified; previews remain optional", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  const files = { "z.html": "z", "index.html": "index", "assets/site.css": "css" };
  const manifest = Object.entries(files).map(([name, content]) => [name, hash(content)]).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
  db.prepare("INSERT INTO artifacts VALUES (?, 1, ?)").run("bundle1", hash(JSON.stringify(manifest)));
  db.prepare("INSERT INTO artifact_revisions VALUES (?, 1, 1, ?)").run("bundle1", hash(JSON.stringify([["index.html", hash("old")]])));
  await Promise.all(Object.entries(files).map(async ([name, content]) => {
    const target = path.join(data, "artifacts", "bundle1", name);
    await fs.mkdir(path.dirname(target), { recursive: true });
    await fs.writeFile(target, content);
  }));
  await fs.mkdir(path.join(data, "artifacts", ".history", "bundle1", "1"), { recursive: true });
  await fs.writeFile(path.join(data, "artifacts", ".history", "bundle1", "1", "index.html"), "old");
  const preview = Buffer.from("preview-cache");
  await fs.mkdir(path.join(data, "previews"), { recursive: true });
  await fs.writeFile(path.join(data, "previews", "bundle1.png"), preview);
  db.close();
  assert.equal(await runBackup(data, backups), 0);
  const [name] = await fs.readdir(backups);
  assert.equal(await fs.readFile(path.join(backups, name, "artifacts", "bundle1", "assets", "site.css"), "utf8"), "css");
  assert.equal(hash(await fs.readFile(path.join(backups, name, "previews", "bundle1.png"))), hash(preview));
});

test("corruption of a copied preview fails closed while preserving the source cache", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  const preview = Buffer.from("preview-cache");
  await fs.mkdir(path.join(data, "previews"), { recursive: true });
  await fs.writeFile(path.join(data, "previews", "bundle1.png"), preview);
  db.close();
  assert.equal(await startBackup(data, backups, undefined, undefined, "14", { BACKUP_TEST_CORRUPT_PREVIEW: "1" }), 1);
  assert.deepEqual(await fs.readdir(backups), []);
  assert.deepEqual((await fs.readdir(backups, { withFileTypes: true })).filter((entry) => entry.name.startsWith(".incomplete-")), []);
  assert.equal(hash(await fs.readFile(path.join(data, "previews", "bundle1.png"))), hash(preview));
});

test("optional previews may disappear after capture without blocking backup", async (t) => {
  const { data, backups, db } = await fixture(t);
  await fs.mkdir(path.join(data, "previews"), { recursive: true });
  await fs.writeFile(path.join(data, "previews", "bundle1.png"), "preview-cache");
  db.close();
  assert.equal(await startBackup(data, backups, undefined, undefined, "14", { BACKUP_TEST_REMOVE_PREVIEWS: "1" }), 0);
  const [name] = await fs.readdir(backups);
  await assert.rejects(fs.access(path.join(backups, name, "previews")), { code: "ENOENT" });
});

test("digest or required-body failure leaves no completed backup; fixing it permits retry", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  const body = Buffer.from("stable");
  db.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("single1", hash(body));
  db.prepare("INSERT INTO artifact_revisions VALUES (?, 1, 0, ?)").run("single1", hash("history"));
  await fs.writeFile(path.join(data, "artifacts", "single1.html"), body);
  db.close();
  const pause = path.join(root, "pause");
  const running = startBackup(data, backups, pause);
  await waitForFile(pause);
  await fs.writeFile(path.join(data, "artifacts", "single1.html"), "corrupt");
  await release(pause);
  assert.notEqual(await running, 0);
  assert.deepEqual(await fs.readdir(backups), []);
  await fs.writeFile(path.join(data, "artifacts", "single1.html"), body);
  await fs.mkdir(path.join(data, "artifacts", ".history", "single1"), { recursive: true });
  await fs.writeFile(path.join(data, "artifacts", ".history", "single1", "1.html"), "history");
  assert.equal(await runBackup(data, backups), 0);
  assert.equal((await fs.readdir(backups)).length, 1);
});

test("missing current or retained bodies and pending intents cannot publish a recovery point", async (t) => {
  const { data, backups, db } = await fixture(t);
  db.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("required", hash("current"));
  db.prepare("INSERT INTO artifact_revisions VALUES (?, 1, 0, ?)").run("required", hash("previous"));
  db.close();
  assert.equal(await runBackup(data, backups), 1);
  assert.deepEqual(await fs.readdir(backups), []);
  await fs.writeFile(path.join(data, "artifacts", "required.html"), "current");
  assert.equal(await runBackup(data, backups), 1);
  assert.deepEqual(await fs.readdir(backups), []);
  await fs.mkdir(path.join(data, "artifacts", ".history", "required"), { recursive: true });
  await fs.writeFile(path.join(data, "artifacts", ".history", "required", "1.html"), "previous");
  const live = new Database(path.join(data, "artifacts.db"));
  live.prepare("INSERT INTO artifact_durability_intents VALUES (?)").run("pending-publish");
  live.close();
  assert.equal(await runBackup(data, backups), 1);
  assert.deepEqual(await fs.readdir(backups), []);
  const source = new Database(path.join(data, "artifacts.db"), { readonly: true });
  assert.equal(source.prepare("SELECT count(*) AS count FROM artifact_durability_intents").get().count, 1);
  source.close();
});

test("backup paths containing SQL and URI punctuation remain valid", async (t) => {
  const { root, data, db } = await fixture(t);
  db.close();
  const backups = path.join(root, "backups ' % # ?");
  assert.equal(await runBackup(data, backups), 0);
  assert.equal((await fs.readdir(backups)).length, 1);
});

test("required bodies cannot depend on symlinks outside the completed backup", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  db.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("linked", hash("external"));
  db.close();
  const external = path.join(root, "external");
  await fs.mkdir(external);
  await fs.writeFile(path.join(external, "linked.html"), "external");
  await fs.rm(path.join(data, "artifacts"), { recursive: true });
  await fs.symlink(external, path.join(data, "artifacts"));
  assert.equal(await runBackup(data, backups), 1);
  assert.deepEqual(await fs.readdir(backups), []);
});

test("concurrent backups have unique names and retention keeps the latest completed snapshot", async (t) => {
  const { data, backups, db } = await fixture(t);
  db.close();
  assert.deepEqual(await Promise.all([runBackup(data, backups), runBackup(data, backups), runBackup(data, backups)]), [0, 0, 0]);
  const older = await fs.readdir(backups);
  assert.equal(older.length, 3);
  assert.equal(await runBackup(data, backups, "1"), 0);
  const latest = await fs.readdir(backups);
  assert.equal(latest.length, 1);
  assert.ok(!older.includes(latest[0]));
});

test("invalid retention and failed publication cannot leave incomplete backups", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  db.close();
  for (const keep of ["0", "00", "-1", "invalid"]) {
    assert.equal(await runBackup(data, backups, keep), 1);
  }
  await assert.rejects(fs.access(backups), { code: "ENOENT" });
  await fs.writeFile(path.join(root, "bin", "mv"), "#!/usr/bin/env bash\nexit 1\n", { mode: 0o755 });
  assert.equal(await runBackup(data, backups), 1);
  assert.deepEqual(await fs.readdir(backups), []);
});

test("the verifier fails closed on omitted paths and required missing databases", async (t) => {
  const { root, db } = await fixture(t);
  db.close();
  const verifier = path.resolve("scripts/backup-coherence.py");
  assert.notEqual(spawnSync("python3", [verifier]).status, 0);
  assert.equal(spawnSync("python3", [verifier, root, "--database-required"]).status, 1);
});

test("deleting a current body after the database cut fails closed and cleans staging", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  db.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("gone", hash("body"));
  await fs.writeFile(path.join(data, "artifacts", "gone.html"), "body");
  db.close();
  const pause = path.join(root, "pause");
  const running = startBackup(data, backups, pause, "after");
  await waitForFile(pause);
  await fs.rm(path.join(data, "artifacts", "gone.html"));
  await release(pause);
  assert.notEqual(await running, 0);
  assert.deepEqual(await fs.readdir(backups), []);
  assert.deepEqual((await fs.readdir(backups, { withFileTypes: true })).filter((entry) => entry.name.startsWith(".incomplete-")), []);
  await assert.rejects(fs.access(path.join(data, "artifacts", "gone.html")), { code: "ENOENT" });
});

test("pruning retained history after the database cut fails closed and cleans staging", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  db.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("history", hash("current"));
  db.prepare("INSERT INTO artifact_revisions VALUES (?, 1, 0, ?)").run("history", hash("old"));
  await fs.writeFile(path.join(data, "artifacts", "history.html"), "current");
  await fs.mkdir(path.join(data, "artifacts", ".history", "history"), { recursive: true });
  const retained = path.join(data, "artifacts", ".history", "history", "1.html");
  await fs.writeFile(retained, "old");
  db.close();
  const pause = path.join(root, "pause");
  const running = startBackup(data, backups, pause, "after");
  await waitForFile(pause);
  await fs.rm(retained);
  await release(pause);
  assert.notEqual(await running, 0);
  assert.deepEqual(await fs.readdir(backups), []);
  assert.deepEqual((await fs.readdir(backups, { withFileTypes: true })).filter((entry) => entry.name.startsWith(".incomplete-")), []);
  await assert.rejects(fs.access(retained), { code: "ENOENT" });
});

test("interrupting after the database cut removes incomplete staging", async (t) => {
  const { root, data, backups, db } = await fixture(t);
  db.prepare("INSERT INTO artifacts VALUES (?, 0, ?)").run("survive", hash("body"));
  await fs.writeFile(path.join(data, "artifacts", "survive.html"), "body");
  db.close();
  const pause = path.join(root, "pause");
  const child = spawn("bash", [script, data, backups, "14"], {
    detached: true,
    env: { ...process.env, PATH: `${path.join(path.dirname(data), "bin")}:${process.env.PATH}`, BACKUP_TEST_PAUSE: pause, BACKUP_TEST_PHASE: "after" },
  });
  t.after(() => { if (child.exitCode === null) { try { process.kill(-child.pid, "SIGKILL"); } catch {} } });
  await waitForFile(pause);
  process.kill(-child.pid, "SIGTERM");
  await new Promise((resolve) => child.once("close", resolve));
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.deepEqual(await fs.readdir(backups), []);
  assert.equal(await fs.readFile(path.join(data, "artifacts", "survive.html"), "utf8"), "body");
});
