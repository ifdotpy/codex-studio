import { execFileSync } from "node:child_process";
import { defineConfig } from "@playwright/test";
import { delimiter, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { browserExecutablePath } from "./tests/playwright.mjs";

const serverSource = fileURLToPath(
  new URL("../../../../workspaces/runtime/apps/server/src/", import.meta.url),
);
process.env.PYTHONPATH = [serverSource, process.env.PYTHONPATH]
  .filter(Boolean)
  .join(delimiter);
const pythonLauncher = fileURLToPath(
  new URL(
    "../../../../workspaces/runtime/apps/server/src/codex_python.py",
    import.meta.url,
  ),
);
const configuredPython =
  process.env.CODEX_AGENTS_PYTHON ||
  process.env.PYTHON ||
  process.env.PYTHON_BIN;
const python =
  configuredPython ||
  execFileSync("python3", [pythonLauncher], { encoding: "utf8" }).trim();
process.env.CODEX_AGENTS_PYTHON = python;
process.env.PYTHON = python;
process.env.PYTHON_BIN = python;
process.env.PATH = [dirname(python), process.env.PATH]
  .filter(Boolean)
  .join(delimiter);

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
