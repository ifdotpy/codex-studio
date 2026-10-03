import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const specs = [
  "draft-render-performance-browser.spec.mjs",
  "ui-responsiveness-browser.spec.mjs",
];
const repo = fileURLToPath(new URL("../../../../", import.meta.url));
const web = join(repo, "web");
const runner = join(web, "node_modules/@playwright/test/cli.js");
const config = join(web, "playwright.config.ts");
const benchmarkGlobalTimeoutMs = 360_000;
const benchmarkCommandTimeoutMs = 390_000;
const requiredPaths = [
  runner,
  config,
  ...specs.map((spec) => join(repo, "tests/client/performance", spec)),
];
const missingPaths = requiredPaths.filter((path) => !existsSync(path));
if (missingPaths.length)
  throw new Error(
    `Prompt composer benchmark is missing: ${missingPaths.join(", ")}`,
  );

for (const spec of specs) {
  const result = spawnSync(
    process.execPath,
    [
      runner,
      "test",
      "--config",
      config,
      "--project=performance",
      "--global-timeout",
      String(benchmarkGlobalTimeoutMs),
      join(repo, "tests/client/performance", spec),
    ],
    {
      cwd: web,
      stdio: "inherit",
      env: { ...process.env, PLAYWRIGHT_INCLUDE_SPECIAL: "1" },
      timeout: benchmarkCommandTimeoutMs,
    },
  );
  if (result.error) throw result.error;
  if (result.signal) throw new Error(`${spec} timed out`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}
