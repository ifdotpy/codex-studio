#!/usr/bin/env node
// Real production Chat settings with a local 261-chat, 301-turn fixture.
// The account smoke fixture supplies isolated endpoints; no live app or state.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { access } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const result = spawnSync(
  process.execPath,
  [join(root, "tests/accounts-ui-smoke.mjs")],
  {
    cwd: root,
    encoding: "utf8",
    env: { ...process.env, SETTINGS_BENCHMARK: "1" },
  },
);
const output = `${result.stdout || ""}${result.stderr || ""}`;
if (result.status !== 0)
  throw new Error(output || `Browser check exited ${result.status}`);
const opening = Number(output.match(/PERF settings-open-ms=([\d.]+)/)?.[1]);
const saving = Number(
  output.match(/PERF settings-change-to-saved-ms=([\d.]+)/)?.[1],
);
assert.ok(
  Number.isFinite(opening) && opening < 150,
  `Panel opens in ${opening} ms`,
);
assert.ok(
  Number.isFinite(saving) && saving < 300,
  `Setting saves in ${saving} ms`,
);
assert.match(output, /PERF state-refreshes-after-save=0/);
const evidence = output.match(/PERF evidence=(.+)/)?.[1]?.trim();
assert.ok(evidence, "Browser fixture reports its screenshot directory");
for (const name of [
  "chat-settings-390.png",
  "chat-settings-1440.png",
  "chat-settings-saved-390.png",
  "chat-settings-saved-1440.png",
])
  await access(join(evidence, name));
console.log(output.trim());
console.log(
  "PASS chat settings autosave, error revert, request identity, timing, no snapshot refresh, and viewport screenshots",
);
