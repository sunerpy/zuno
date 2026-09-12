import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "./tests",
  testMatch: "*.spec.mjs",
  workers: 1,
  timeout: 30000,
  outputDir: "../../target/enterprise-validation/web-test-results",
  use: { browserName: "chromium", headless: true },
});
