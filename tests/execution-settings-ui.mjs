#!/usr/bin/env node
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium, _electron } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-execution-settings-ui-"));
const models = [
  {
    model: "gpt-6-astra",
    displayName: "Astra",
    levels: ["low", "medium", "high", "ultra"],
  },
  {
    model: "gpt-5.6-sol",
    displayName: "Sol",
    levels: ["low", "medium", "high", "ultra"],
  },
  {
    model: "gpt-5.6-luna",
    displayName: "Luna",
    levels: ["low", "medium", "high"],
  },
  { model: "test-slow", displayName: "Standard only", levels: ["low"] },
].map(({ levels, ...row }) => ({
  ...row,
  defaultReasoningEffort: "low",
  supportedReasoningEfforts: levels.map((reasoningEffort) => ({
    reasoningEffort,
  })),
  serviceTiers:
    row.model === "test-slow"
      ? []
      : [
          {
            id: "priority",
            name: "Fast",
            description: "Faster responses, increased usage",
          },
        ],
}));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, EXECUTION_SETTINGS_CATALOG: JSON.stringify(models) },
  },
);
let browser,
  log = "";
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const url = `http://127.0.0.1:${port}`;
  let page;
  if (process.env.CODEX_TEST_DESKTOP) {
    browser = await _electron.launch({
      executablePath: process.env.CODEX_TEST_DESKTOP,
      args: ["--hidden"],
      env: {
        ...process.env,
        CODEX_AGENTS_STATE_DIR: root,
        CODEX_DESKTOP_PORT: String(port),
        CODEX_DESKTOP_PROFILE: join(root, "desktop-profile"),
      },
    });
    page = await browser.firstWindow();
    assert.equal(
      await browser.evaluate(({ BrowserWindow }) =>
        BrowserWindow.getAllWindows()[0].isVisible(),
      ),
      false,
    );
  } else {
    browser = await chromium.launch({
      headless: true,
      executablePath:
        process.env.CHROME_BIN ||
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    });
    page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
  }
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  if (process.env.CODEX_TEST_DESKTOP) {
    await page.waitForURL(url + "/");
    await page.locator("#message").waitFor();
  } else {
    await page.goto(url);
  }
  const state = async () =>
    (await (await fetch(url + "/api/state")).json()).runtime.agents;
  const waitFor = async (predicate) => {
    for (let i = 0; i < 100; i++) {
      const result = await predicate();
      if (result) return result;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error("Timed out: " + log);
  };
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const lead = (await state()).find((a) => a.name === "Release lead");
  await page
    .getByRole("button", { name: "Lead settings", exact: true })
    .click();
  await page.getByLabel("Lead reasoning", { exact: true }).selectOption("high");
  await waitFor(
    async () => (await state()).find((a) => a.id === lead.id).effort === "high",
  );
  const yolo = page.getByRole("switch", { name: "YOLO mode", exact: true });
  await yolo.uncheck();
  await page
    .getByRole("alert")
    .filter({ hasText: "Wait for every team turn" })
    .waitFor();
  assert.equal((await state()).find((a) => a.id === lead.id).yoloMode, true);
  await waitFor(() => yolo.isChecked());
  await page.getByRole("switch", { name: "Fast mode", exact: true }).check();
  await waitFor(
    async () => (await state()).find((a) => a.id === lead.id).fastMode === true,
  );
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Subagent defaults", exact: true })
    .click();
  const dialog = page.getByRole("dialog", {
    name: "Subagent defaults",
    exact: true,
  });
  await dialog
    .getByLabel("Default subagent model", { exact: true })
    .selectOption("gpt-5.6-luna");
  assert.equal(
    await dialog
      .getByLabel("Default subagent reasoning")
      .locator('option[value="ultra"]')
      .count(),
    0,
  );
  await dialog.getByLabel("Default subagent reasoning").selectOption("high");
  await dialog.getByRole("switch", { name: "Fast mode", exact: true }).check();
  for (const width of [1440, 390, 320]) {
    await page.setViewportSize({ width, height: 960 });
    if (width === 390) {
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      await page
        .getByRole("button", { name: "Subagent defaults", exact: true })
        .click();
    }
    assert.ok(
      await dialog.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
    );
    await page.screenshot({ path: join(root, `defaults-${width}.png`) });
  }
  await waitFor(
    async () =>
      !(await dialog.getByLabel("Default subagent model").isDisabled()),
  );
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  let current = (await state()).find((a) => a.id === lead.id);
  assert.deepEqual(current.workerDefaults, {
    model: "gpt-5.6-luna",
    effort: "high",
    fastMode: true,
  });
  const spawnWorker = async (name, overrides = {}) => {
    proc.stdin.write(
      JSON.stringify({
        method: "fixture/create-worker",
        parent: lead.id,
        params: {
          name,
          prompt: "Review without running",
          role: "reviewer",
          ...overrides,
        },
      }) + "\n",
    );
    return waitFor(async () => (await state()).find((a) => a.name === name));
  };
  const first = await spawnWorker("Inherits defaults");
  assert.equal(first.model, "gpt-5.6-luna");
  assert.equal(first.effort, "high");
  assert.equal(first.fastMode, true);
  const overridden = await spawnWorker("Orchestrator override", {
    model: "gpt-5.6-sol",
    effort: "low",
    fast_mode: false,
  });
  assert.equal(overridden.model, "gpt-5.6-sol");
  assert.equal(overridden.effort, "low");
  assert.equal(overridden.fastMode, false);
  await page.setViewportSize({ width: 1440, height: 960 });
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Subagent defaults", exact: true })
    .click();
  assert.equal(
    await dialog.getByLabel("Default subagent model").inputValue(),
    "gpt-5.6-luna",
  );
  await dialog.getByLabel("Default subagent model").selectOption("test-slow");
  assert.ok(
    await dialog
      .getByRole("switch", { name: "Fast mode", exact: true })
      .isDisabled(),
  );
  assert.equal(
    await dialog
      .getByRole("switch", { name: "Fast mode", exact: true })
      .isChecked(),
    false,
  );
  assert.equal(
    await dialog.getByLabel("Default subagent reasoning").inputValue(),
    "__model_default__",
  );
  await waitFor(
    async () =>
      (await state()).find((a) => a.id === lead.id).workerDefaults.model ===
      "test-slow",
  );
  await dialog.getByLabel("Default subagent model").selectOption("gpt-5.6-sol");
  await dialog.getByLabel("Default subagent reasoning").selectOption("low");
  await dialog
    .getByRole("switch", { name: "Fast mode", exact: true })
    .uncheck();
  await waitFor(
    async () =>
      !(await dialog.getByLabel("Default subagent model").isDisabled()),
  );
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  const next = await spawnWorker("New defaults");
  assert.equal(next.model, "gpt-5.6-sol");
  assert.equal(next.effort, "low");
  assert.equal(next.fastMode, false);
  assert.equal(
    (await state()).find((a) => a.id === first.id).model,
    "gpt-5.6-luna",
  );
  await page.screenshot({ path: join(root, "team-defaults.png") });
  for (const width of [1440, 768, 390, 320]) {
    await page.setViewportSize({ width, height: 960 });
    if (width === 390) {
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      await page
        .getByRole("button", { name: "Subagent defaults", exact: true })
        .click();
    }
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth + 1,
      ),
      "header fits " + width,
    );
  }
  await page.setViewportSize({ width: 1440, height: 960 });
  const snapshot = await (await fetch(url + "/api/state")).json();
  const freshResponse = await fetch(url + "/api/leads", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Origin: url,
      "X-Canvas-Token": snapshot.token,
    },
    body: JSON.stringify({ cwd: root }),
  });
  assert.equal(freshResponse.status, 200);
  const fresh = await freshResponse.json();
  await page.reload();
  await page.locator(`[data-chat="${fresh.id}"]`).click();
  await page
    .getByRole("button", { name: "Lead settings", exact: true })
    .click();
  const freshYolo = page.getByRole("switch", {
    name: "YOLO mode",
    exact: true,
  });
  assert.equal(await freshYolo.isChecked(), true);
  await freshYolo.uncheck();
  await waitFor(
    async () =>
      (await state()).find((a) => a.id === fresh.id).yoloMode === false,
  );
  await waitFor(async () => !(await freshYolo.isDisabled()));
  await waitFor(async () => !(await freshYolo.isChecked()));
  await freshYolo.check();
  await waitFor(
    async () =>
      (await state()).find((a) => a.id === fresh.id).yoloMode === true,
  );
  await waitFor(async () => !(await freshYolo.isDisabled()));
  await page.screenshot({ path: join(root, "lead-yolo.png") });
  assert.deepEqual(errors, []);
  console.log(
    "PASS execution settings UI: lead reasoning/Fast, per-model choices, persisted defaults, actual child inheritance and orchestrator overrides, unsupported Fast, immediate save, existing worker unchanged, responsive layout. Evidence " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
