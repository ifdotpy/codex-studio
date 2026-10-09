import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const web = fileURLToPath(new URL("../../../", import.meta.url));
const testFiles = [
  "src/components/prompt-composer/PromptInput.spec.mjs",
  "tests/composer/composer-stability-ui.spec.mjs",
  "tests/composer/skill-autocomplete-ui.spec.mjs",
  "tests/performance/draft-render-performance-browser.spec.mjs",
];
const missingTests = testFiles.filter((test) => !existsSync(join(web, test)));
if (missingTests.length > 0) {
  throw new Error(
    `Prompt composer Playwright specs are missing:\n${missingTests.join("\n")}`,
  );
}

const verificationTimeoutMs = 16 * 60 * 1000;
const result = spawnSync(
  "pnpm",
  [
    "exec",
    "playwright",
    "test",
    "--config",
    "playwright.config.ts",
    "--project=client",
    "--project=performance",
    ...testFiles,
    ...process.argv.slice(2),
  ],
  {
    cwd: web,
    stdio: "inherit",
    env: { ...process.env, PLAYWRIGHT_INCLUDE_SPECIAL: "1" },
    timeout: verificationTimeoutMs,
  },
);
if (result.error) throw result.error;
if (result.signal)
  throw new Error("Prompt composer Playwright check timed out");
if (result.status !== 0) process.exit(result.status ?? 1);
