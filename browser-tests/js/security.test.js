import { test, assert, readText } from "./harness.js";

test("Docker build context excludes deployment secrets and persistent data", async () => {
  const patterns = (await readText(".dockerignore"))
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith("#"));

  for (const required of [".env", ".env.*", "!.env.example", "data/", ".git/", ".local/", "node_modules/"]) {
    assert.ok(patterns.includes(required), `.dockerignore must exclude ${required}`);
  }
});
