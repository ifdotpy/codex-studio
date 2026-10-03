import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const testFiles = [
  "PromptInput.spec.mjs",
  "composer-stability-ui.spec.mjs",
  "skill-autocomplete-ui.spec.mjs",
  "draft-render-performance-browser.spec.mjs",
];
const web = fileURLToPath(new URL("../../../", import.meta.url));
const require = createRequire(
  new URL("../../../package.json", import.meta.url),
);
const playwrightCli = require.resolve("@playwright/test/cli");

function run(command, args, cwd, env = process.env) {
  const result = spawnSync(command, args, {
    cwd,
    stdio: "inherit",
    env,
    timeout: 90_000,
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
