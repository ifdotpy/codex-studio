import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const tests = [
  "draft-render-performance-browser.mjs",
  "ui-responsiveness-browser.mjs",
];
const root = fileURLToPath(new URL("../../../../tests/", import.meta.url));

for (const test of tests) {
  const result = spawnSync(process.execPath, [`${root}${test}`], {
    stdio: "inherit",
    env: process.env,
    timeout: 90_000,
  });
  if (result.error) throw result.error;
  if (result.signal) throw new Error(`${test} timed out`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}
