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
  const openSettings = async (name) => {
    if (!(await page.getByRole("button", { name, exact: true }).isVisible()))
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
    await page.getByRole("button", { name, exact: true }).click();
  };
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const lead = (await state()).find((a) => a.name === "Release lead");
  await openSettings("Main agent settings");
  await page
    .getByLabel("Main agent reasoning", { exact: true })
    .selectOption("high");
  await waitFor(
    async () => (await state()).find((a) => a.id === lead.id).effort === "high",
  );
  await page.getByText("Permissions", { exact: true }).click();
  const yolo = page.getByRole("switch", {
    name: "Full access without approval",
    exact: true,
  });
  await yolo.click();
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
    .getByRole("dialog", { name: "Main agent settings", exact: true })
    .waitFor({ state: "hidden" });
  assert.ok(
    await page
      .getByRole("dialog", { name: "Chat settings", exact: true })
      .isVisible(),
    "Escape closes only the model popover",
  );
  await openSettings("Subagent defaults");
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
  await openSettings("Subagent defaults");
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
  await dialog
    .getByRole("status")
    .filter({ hasText: "Fast mode is unavailable and was turned off" })
    .waitFor();
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
  await openSettings("Main agent settings");
  await page.getByText("Permissions", { exact: true }).click();
  const freshYolo = page.getByRole("switch", {
    name: "Full access without approval",
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
  await page.keyboard.press("Escape");
  const chatSettings = page.getByRole("dialog", {
    name: "Chat settings",
    exact: true,
  });
  if (await chatSettings.isVisible())
    await chatSettings.locator(".mantine-Modal-close").click();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  const running = (await state()).find((agent) => agent.name === "Worker 00");
  const openWorker = async () => {
    const row = page.locator(`[data-worker="${running.id}"]`);
    if (!(await row.isVisible()))
      await page.getByRole("button", { name: "Team", exact: true }).click();
    await row.click();
  };
  await openWorker();
  await openSettings("Subagent settings");
  const pendingBodies = [];
  await page.route("**/api/conversation", async (route) => {
    const body = route.request().postDataJSON();
    if (!body.next_turn) {
      await route.continue();
      return;
    }
    pendingBodies.push(body);
    if (pendingBodies.length === 1) {
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      await route.abort("failed");
    } else await route.continue();
  });
  await page
    .getByLabel("Subagent reasoning", { exact: true })
    .selectOption("medium");
  await page
    .getByRole("button", { name: "Check settings save", exact: true })
    .waitFor();
  assert.equal(
    (await state()).find((agent) => agent.id === running.id).effort,
    running.effort,
    "The active turn keeps its reasoning",
  );
  assert.equal(
    (await state()).find((agent) => agent.id === running.id).pendingSettings
      .effort,
    "medium",
  );
  await page.reload();
  // The unconfirmed operation survives a reload and return to the same worker.
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await openWorker();
  await openSettings("Subagent settings");
  await page
    .getByRole("button", { name: "Check settings save", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Check settings save", exact: true })
    .waitFor({ state: "hidden" });
  await waitFor(() => pendingBodies.length === 2);
  assert.deepEqual(
    pendingBodies[1],
    pendingBodies[0],
    "Recovery uses the same settings and request identity",
  );
  assert.match(pendingBodies[0].request_id, /^[0-9a-f-]{36}$/);
  assert.equal(
    (await state()).find((agent) => agent.id === running.id).effort,
    running.effort,
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS execution settings UI: main agent reasoning/Fast, model choices, saved defaults, child inheritance and overrides, dependent reset notice, immediate save, permission guards, nested Escape, next-turn settings, lost-response replay across reload, active worker unchanged, responsive layout. Evidence " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
