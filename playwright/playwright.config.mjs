// Drives two running release-candidate instances. All mutations happen inside a throwaway
// `pwtest-<runid>` organization that global teardown removes.
import { defineConfig } from "@playwright/test";

const NODE_URL = process.env.PW_NODE_URL || "http://127.0.0.1:3485";
const RUST_URL = process.env.PW_RUST_URL || "http://127.0.0.1:3483";

export default defineConfig({
  testDir: "./tests",
  timeout: 30000,
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  globalSetup: "./global-setup.mjs",
  globalTeardown: "./global-teardown.mjs",
  use: {
    extraHTTPHeaders: {
      "Cf-Access-Authenticated-User-Email": process.env.PW_ADMIN_EMAIL || "admin@example.test"
    },
    headless: true,
    ...(process.env.PW_USE_BUNDLED_CHROMIUM === "1" ? {} : { channel: "chrome" }),
    ignoreHTTPSErrors: true,
  },
  projects: [
    { name: "node", use: { baseURL: NODE_URL } },
    { name: "rust", use: { baseURL: RUST_URL } },
  ],
});
