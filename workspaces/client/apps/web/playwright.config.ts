import { defineConfig } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { browserExecutablePath } from "./tests/playwright.mjs";

const workerLimit = 2;
const globalTimeoutMs = 15 * 60 * 1000;
const defaultTestTimeoutMs = 30_000;
const requestedBrowser = process.env.BROWSER ?? "chromium";
if (requestedBrowser !== "chromium" && requestedBrowser !== "webkit")
  throw new Error(`Unsupported BROWSER value: ${requestedBrowser}`);
const browserName: "chromium" | "webkit" = requestedBrowser;

const browserUse = {
  browserName,
  headless: true,
  trace: "retain-on-failure" as const,
  ...(browserName === "chromium"
    ? { launchOptions: { executablePath: browserExecutablePath } }
    : {}),
};

const specialProjects =
  process.env.PLAYWRIGHT_INCLUDE_SPECIAL === "1"
    ? [
        { name: "installed-only", grep: /@live/ },
        { name: "performance", grep: /@performance/ },
      ]
    : [];

export default defineConfig({
  testDir: ".",
  testMatch: [
    fileURLToPath(new URL("./tests/**/*.spec.mjs", import.meta.url)),
    fileURLToPath(new URL("./src/**/*.spec.mjs", import.meta.url)),
  ],
  testIgnore: [
    fileURLToPath(new URL("../../../../.worktrees/**", import.meta.url)),
  ],
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  workers: workerLimit,
  globalTimeout: globalTimeoutMs,
  timeout: defaultTestTimeoutMs,
  reporter: [
    ["list"],
    ["html", { outputFolder: "playwright-report", open: "never" }],
    ["json", { outputFile: "test-results/results.json" }],
  ],
  projects: [
    {
      name: "client",
      grepInvert: /@live|@performance/,
      use: browserUse,
    },
    ...specialProjects.map(({ name, grep }) => ({
      name,
      grep,
      use: browserUse,
    })),
  ],
});
