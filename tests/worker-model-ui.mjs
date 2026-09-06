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
  } else {
    await page.goto(url);
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
          { model: "ui-only", displayName: "No longer available" },
          { model: "hidden-model", hidden: true },
        ],
      },
    });
  });
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page
    .getByRole("button", { name: "Retry model list" })
    .first()
    .waitFor();
  assert.ok(
    await page.getByLabel("Model for Worker 07", { exact: true }).isDisabled(),
  );
  await page.getByRole("button", { name: "Retry model list" }).first().click();
  const selector = page.getByLabel("Model for Worker 07", { exact: true });
  await page.waitForFunction(
    () =>
      !document.querySelector('[aria-label="Model for Worker 07"]').disabled,
  );
  assert.equal(
    await selector.locator('option[value="hidden-model"]').count(),
    0,
  );
  await selector.selectOption("test-model");
  await page.waitForFunction(
    () =>
      document.querySelector('[aria-label="Model for Worker 07"]').value ===
      "test-model",
  );
  const state = await (await fetch(url + "/api/state")).json();
  const worker = state.runtime.agents.find((a) => a.name === "Worker 07");
  assert.equal(worker.model, "test-model");
  await page.locator(`[data-worker="${worker.id}"]`).click();
  await page.locator("#model").waitFor();
  assert.equal(await page.locator("#model").inputValue(), "test-model");
  await page.locator("#model").selectOption("ui-only");
  await page
    .getByText("This model is not available for the subagent account", {
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
  await page.locator(`[data-worker="${busyWorker.id}"]`).click();
  assert.ok(await page.locator("#model").isDisabled());
  await page.locator("#back-lead").click();
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
    "PASS worker model UI: catalog retry, hidden models, persisted worker selection, rejected stale model, busy guard, lead restrictions, shared catalog, responsive layouts. Evidence " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
