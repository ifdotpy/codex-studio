import { defineConfig } from "@playwright/test";
import { browserExecutablePath } from "../tests/client/playwright.mjs";

const workerLimit = 2;
const globalTimeoutMs = 15 * 60 * 1000;
const defaultTestTimeoutMs = 30_000;

const specialProjects =
  process.env.PLAYWRIGHT_INCLUDE_SPECIAL === "1"
    ? [
        { name: "installed-only", grep: /@live/ },
        { name: "performance", grep: /@performance/ },
      ]
    : [];

export default defineConfig({
  testDir: "..",
  testMatch: ["tests/client/**/*.spec.mjs", "web/src/**/*.spec.mjs"],
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
      use: {
        browserName: "chromium",
        headless: true,
        trace: "retain-on-failure",
        launchOptions: { executablePath: browserExecutablePath },
      },
    },
    ...specialProjects.map(({ name, grep }) => ({
      name,
      grep,
      use: {
        browserName: "chromium" as const,
        headless: true,
        trace: "retain-on-failure" as const,
        launchOptions: { executablePath: browserExecutablePath },
      },
    })),
  ],
});
