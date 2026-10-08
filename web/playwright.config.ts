import { defineConfig } from "@playwright/test";
import { rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { browserExecutablePath } from "../tests/client/browser-executable.mjs";
import { choosePlaywrightScratchRoot } from "../tests/client/scratch-root.mjs";
import { getPlaywrightWorkerSelection } from "./playwright-workers.mjs";

const workerSelection = getPlaywrightWorkerSelection();
const workerLimit = workerSelection.workers;
// MIMALLOC purge and bounded Vite discovery each roughly halved UX Chat worker PSS (2,165 -> 1,171 MiB).
process.env.MIMALLOC_PURGE_DELAY ??= "0";
process.env.MIMALLOC_ARENA_EAGER_COMMIT ??= "0";
process.env.MIMALLOC_ALLOW_LARGE_OS_PAGES ??= "0";
process.env.CODEX_PLAYWRIGHT_SELECTED_WORKERS = String(workerLimit);
const playwrightScratch = choosePlaywrightScratchRoot(process.env, workerLimit);
process.env.TMPDIR = playwrightScratch.root;
process.env.CODEX_PLAYWRIGHT_SCRATCH_PARENT = playwrightScratch.root;
if (playwrightScratch.ownedRoot)
  process.once("exit", () =>
    rmSync(playwrightScratch.root, { recursive: true, force: true }),
  );
if (!workerSelection.cached)
  console.log(
    `Playwright worker bound=${workerSelection.bound} ${JSON.stringify(workerSelection)}`,
  );
console.log(
  `Playwright scratch ${JSON.stringify({
    root: playwrightScratch.root,
    memoryBacked: playwrightScratch.memoryBacked,
    mount: playwrightScratch.mount,
    availableBytes: playwrightScratch.availableBytes,
    probeBytes: playwrightScratch.probeBytes,
    writeFsyncMs: playwrightScratch.writeFsyncMs,
    tested: playwrightScratch.tested,
  })}`,
);
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
  globalTeardown: fileURLToPath(
    new URL("./playwright-memory-teardown.mjs", import.meta.url),
  ),
  testDir: "..",
  testMatch: [
    fileURLToPath(new URL("../tests/client/**/*.spec.mjs", import.meta.url)),
    fileURLToPath(new URL("./src/**/*.spec.mjs", import.meta.url)),
  ],
  testIgnore: [fileURLToPath(new URL("../.worktrees/**", import.meta.url))],
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  workers: workerLimit,
  globalTimeout: globalTimeoutMs,
  timeout: defaultTestTimeoutMs,
  reporter: [["list"], ["json", { outputFile: "test-results/results.json" }]],
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
