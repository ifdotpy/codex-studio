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
const root = await mkdtemp(join(tmpdir(), "codex-worker-model-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
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
  }
  let requests = 0;
  await page.route("**/api/models?**", async (route) => {
    requests++;
    if (requests === 1)
      return route.fulfill({
        status: 503,
        json: { error: "Model catalog unavailable" },
      });
    return route.fulfill({
      json: {
        data: [
          { model: "test-model", displayName: "Test model" },
          { model: "gpt-6-astra" },
          { model: "gpt-5.6-sol" },
          { model: "ui-only", displayName: "No longer available" },
          { model: "hidden-model", hidden: true },
        ],
      },
    });
  });
  if (process.env.CODEX_TEST_DESKTOP) await page.reload();
  else await page.goto(url);
  const settings = page.getByRole("dialog", {
    name: "Chat settings",
    exact: true,
  });
  const openSettings = async (name) => {
    if (!(await settings.isVisible()))
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
    await page.getByRole("button", { name, exact: true }).click();
  };
  const closeSettings = async () => {
    await page.keyboard.press("Escape");
    await settings.locator(".mantine-Modal-close").click();
    await settings.waitFor({ state: "hidden" });
  };
  const openWorker = async (id) => {
    const row = page.locator(`[data-worker="${id}"]`);
    if (!(await row.isVisible()))
      await page.getByRole("button", { name: "Team", exact: true }).click();
    await row.click();
  };
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const initialState = await (await fetch(url + "/api/state")).json();
  const worker = initialState.runtime.agents.find(
    (agent) => agent.name === "Worker 07",
  );
  await openWorker(worker.id);
  await openSettings("Subagent settings");
  await page.getByRole("button", { name: "Retry model list" }).waitFor();
  assert.ok(await page.locator("#model").isDisabled());
  await page.getByRole("button", { name: "Retry model list" }).click();
  const selector = page.locator("#model");
  await page.waitForFunction(() => !document.querySelector("#model").disabled);
  assert.equal(
    await selector.locator('option[value="hidden-model"]').count(),
    0,
  );
  await selector.selectOption("test-model");
  await page.waitForFunction(
    () =>
      !document.querySelector("#model").disabled &&
      document.querySelector("#model").value === "test-model",
  );
  const state = await (await fetch(url + "/api/state")).json();
  assert.equal(
    state.runtime.agents.find((agent) => agent.id === worker.id).model,
    "test-model",
  );
  await page.locator("#model").selectOption("ui-only");
  await page
    .getByText("This model is not available for this account", {
      exact: true,
    })
    .waitFor();
  assert.equal(await page.locator("#model").inputValue(), "test-model");
  for (const width of [1440, 768, 420]) {
    await page.setViewportSize({ width, height: 960 });
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth + 1,
      ),
      "layout fits " + width,
    );
    await page.screenshot({ path: join(root, `worker-model-${width}.png`) });
  }
  await page.setViewportSize({ width: 1440, height: 960 });
  const busyWorker = state.runtime.agents.find((a) => a.name === "Worker 01");
  await closeSettings();
  await openWorker(busyWorker.id);
  await openSettings("Subagent settings");
  assert.equal(await page.locator("#model").isDisabled(), false);
  await page
    .getByText("Model settings apply to the next turn.", { exact: false })
    .waitFor();
  await page.locator("#model").selectOption("test-model");
  await page.waitForFunction(async (id) => {
    const snapshot = await fetch("/api/state").then((response) =>
      response.json(),
    );
    return (
      snapshot.runtime.agents.find((agent) => agent.id === id)?.pendingSettings
        ?.model === "test-model"
    );
  }, busyWorker.id);
  const queuedState = await (await fetch(url + "/api/state")).json();
  assert.equal(
    queuedState.runtime.agents.find((agent) => agent.id === busyWorker.id)
      .model,
    busyWorker.model,
    "The active worker keeps its current model",
  );
  await closeSettings();
  await page.locator("#back-lead").click();
  await openSettings("Main agent settings");
  await page.locator("#model").waitFor();
  assert.deepEqual(
    await page
      .locator("#model option")
      .evaluateAll((nodes) => nodes.map((n) => n.value)),
    ["gpt-6-astra", "gpt-5.6-sol"],
  );
  assert.equal(
    requests,
    2,
    "one account catalog is shared by the worker list and chat",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS worker model UI: catalog retry, hidden models, persisted worker selection, rejected stale model, next-turn selection preserves active model, lead restrictions, shared catalog, responsive layouts. Evidence " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
