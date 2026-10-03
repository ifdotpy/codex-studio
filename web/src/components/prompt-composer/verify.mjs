import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const testFiles = [
  "web/src/components/prompt-composer/PromptInput.spec.mjs",
  "tests/client/composer/composer-stability-ui.spec.mjs",
  "tests/client/composer/skill-autocomplete-ui.spec.mjs",
  "tests/client/performance/draft-render-performance-browser.spec.mjs",
];
const root = fileURLToPath(new URL("../../../../", import.meta.url));
const web = fileURLToPath(new URL("../../../", import.meta.url));
const require = createRequire(
  new URL("../../../package.json", import.meta.url),
);
const playwrightCli = require.resolve("@playwright/test/cli");
const verificationTimeoutMs = 16 * 60 * 1000;

const missingTests = testFiles.filter((test) => !existsSync(join(root, test)));
if (missingTests.length > 0) {
  throw new Error(
    `Prompt composer Playwright specs are missing:\n${missingTests.join("\n")}`,
  );
}

function run(command, args, cwd, env = process.env) {
  const result = spawnSync(command, args, {
    cwd,
    stdio: "inherit",
    env,
    timeout: verificationTimeoutMs,
  });
  if (result.error) throw result.error;
  if (result.signal) throw new Error(`${args.join(" ")} timed out`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

run(
  process.execPath,
  [
    playwrightCli,
    "test",
    "--config",
    "playwright.config.ts",
    "--project=client",
    "--project=performance",
    ...testFiles,
  ],
  web,
  { ...process.env, PLAYWRIGHT_INCLUDE_SPECIAL: "1" },
);
