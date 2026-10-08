import {
  readTestState,
  installApiSchemaFixtureHeaders,
  test,
  browserExecutablePath,
  spawnFixture as spawn,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { modelOptions, modelValue, selectModel } from "../../model-picker.mjs";
test("Worker model", async () => {
  test.setTimeout(60_000);
  const testRepo = fileURLToPath(new URL("../../../", import.meta.url));
  // Production client with an isolated runtime. No model service or user state.
  const skill = testRepo;
  const { chromium, _electron } = createRequire(
    join(skill, "web/package.json"),
  )("playwright-core");
  const root = await mkdtemp(join(tmpdir(), "codex-worker-model-ui-"));
  const proc = spawn(
    process.env.PYTHON || "python3",
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
        executablePath: browserExecutablePath,
      });
      page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
    }
    await page.exposeFunction("__readTestState", () => readTestState(url));
    installApiSchemaFixtureHeaders(page);
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    if (process.env.CODEX_TEST_DESKTOP) {
      await page.waitForURL(url + "/");
      await page.locator("#message").waitFor();
    }
    let requests = 0;
    let workerRequests = 0;
    await page.route("**/api/models?**", async (route) => {
      if (new URL(route.request().url()).searchParams.get("workers") === "1") {
        workerRequests++;
        return route.fulfill({
          json: {
            data: [
              {
                model: "gpt-6-luna",
                displayName: "Luna",
                provider: "codex",
                accountKey: "other-codex",
                supportedReasoningEfforts: [{ reasoningEffort: "high" }],
              },
            ],
          },
        });
      }
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
      const role = name === "Subagent defaults" ? "Worker" : undefined;
      const panelName = role ? "Main agent settings" : name;
      await settings
        .getByRole("button", { name: panelName, exact: true })
        .click();
      let panel = page.getByRole("dialog", {
        name: panelName,
        exact: true,
      });
      await panel.waitFor({ state: "visible" });
      if (role) {
        await panel
          .getByRole("group", { name: "Agent role" })
          .getByRole("button", { name: role, exact: true })
          .click();
        panel = page.getByRole("dialog", { name, exact: true });
        await panel.waitFor({ state: "visible" });
      }
      assert.ok(await settings.isVisible(), "Chat settings remain open");
      assert.ok(await panel.isVisible(), "Model settings dialog is visible");
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
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    const initialState = await readTestState(url);
    const worker = initialState.runtime.agents.find(
      (agent) => agent.name === "Worker 07",
    );
    await openWorker(worker.id);
    await openSettings("Subagent settings");
    await page.getByRole("button", { name: "Retry model list" }).waitFor();
    const selector = page
      .getByRole("dialog", { name: "Subagent settings", exact: true })
      .locator("#model");
    assert.deepEqual(
      await selector
        .getByRole("option")
        .evaluateAll((options) =>
          options.map((option) => option.getAttribute("data-value")),
        ),
      [],
      "A failed model catalog exposes no selectable choices",
    );
    await page.getByRole("button", { name: "Retry model list" }).click();
    await page
      .getByRole("button", { name: "Retry model list" })
      .waitFor({ state: "hidden" });
    assert.ok(
      await selector.getByRole("option", { name: /Test model/ }).isVisible(),
    );
    assert.equal(
      (await modelOptions(selector)).includes("hidden-model"),
      false,
    );
    await selectModel(selector, "test-model");
    await page.waitForFunction(
      () =>
        !document.querySelector("#model").disabled &&
        document.querySelector("#model").dataset.value === "test-model",
    );
    await page.waitForFunction(async (id) => {
      const snapshot = await window.__readTestState();
      const selected = snapshot.runtime.agents.find((agent) => agent.id === id);
      return selected?.model === "test-model";
    }, worker.id);
    const state = await readTestState(url);
    const selectedWorker = state.runtime.agents.find(
      (agent) => agent.id === worker.id,
    );
    assert.equal(
      selectedWorker.model,
      "test-model",
      "An idle worker's selected model is persisted immediately",
    );
    await selectModel(selector, "ui-only");
    await page
      .getByText("This model is not available for this account", {
        exact: true,
      })
      .waitFor();
    assert.equal(await modelValue(selector), "test-model");
    for (const width of [1440, 768, 420]) {
      await page.setViewportSize({ width, height: 960 });
      await page.waitForFunction(
        (expectedWidth) =>
          new Promise((resolve) => {
            let previous = "";
            let stableFrames = 0;
            const sample = () => {
              const dialog = document.querySelector('[role="dialog"]');
              const rect = dialog?.getBoundingClientRect();
              const values = [
                innerWidth,
                document.documentElement.scrollWidth,
                document.body.scrollWidth,
                rect?.x,
                rect?.y,
                rect?.width,
                rect?.height,
              ].join(":");
              stableFrames =
                innerWidth === expectedWidth && values === previous
                  ? stableFrames + 1
                  : 0;
              previous = values;
              if (stableFrames >= 3) resolve(true);
              else requestAnimationFrame(sample);
            };
            requestAnimationFrame(sample);
          }),
        width,
      );
      assert.ok(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth + 1,
        ),
        "layout fits " + width,
      );
    }
    await page.setViewportSize({ width: 1440, height: 960 });
    const busyWorker = state.runtime.agents.find((a) => a.name === "Worker 01");
    await closeSettings();
    await openWorker(busyWorker.id);
    await openSettings("Subagent settings");
    const busyWorkerModel = page
      .getByRole("dialog", { name: "Subagent settings", exact: true })
      .locator("#model");
    assert.equal(await busyWorkerModel.getAttribute("aria-disabled"), null);
    await page
      .getByText("Model settings apply to the next turn.", { exact: false })
      .waitFor();
    await selectModel(busyWorkerModel, "test-model");
    await page.waitForFunction(async (id) => {
      const snapshot = await window.__readTestState();
      return (
        snapshot.runtime.agents.find((agent) => agent.id === id)
          ?.pendingSettings?.model === "test-model"
      );
    }, busyWorker.id);
    const queuedState = await readTestState(url);
    assert.equal(
      queuedState.runtime.agents.find((agent) => agent.id === busyWorker.id)
        .model,
      busyWorker.model,
      "The active worker keeps its current model",
    );
    await closeSettings();
    await page.locator("#back-lead").click();
    await openSettings("Main agent settings");
    const leadModel = page
      .getByRole("dialog", { name: "Main agent settings", exact: true })
      .locator("#model");
    await leadModel.waitFor();
    assert.deepEqual(await modelOptions(leadModel), [
      "test-model",
      "gpt-6-astra",
      "gpt-5.6-sol",
      "ui-only",
    ]);
    await selectModel(leadModel, "test-model");
    await page.waitForFunction(
      () =>
        !document.querySelector("#model").disabled &&
        document.querySelector("#model").dataset.value === "test-model",
    );
    const leadState = await readTestState(url);
    const selectedLead = leadState.runtime.agents.find(
      (row) => row.name === "Release lead",
    );
    assert.equal(
      selectedLead.pendingSettings?.model || selectedLead.model,
      "test-model",
    );
    assert.equal(
      requests,
      2,
      "one account catalog is shared by the worker list and chat",
    );
    await page.keyboard.press("Escape");
    await openSettings("Subagent defaults");
    const defaults = page.getByRole("dialog", {
      name: "Subagent defaults",
      exact: true,
    });
    assert.equal(
      await modelValue(
        defaults.getByLabel("Default subagent model", { exact: true }),
      ),
      "gpt-6-luna",
    );
    assert.equal(
      await defaults
        .getByRole("radiogroup", {
          name: "Default subagent reasoning",
          exact: true,
        })
        .getAttribute("data-value"),
      "high",
    );
    assert.equal(
      (
        await modelOptions(
          defaults.getByLabel("Default subagent model", { exact: true }),
        )
      ).includes("ui-only"),
      false,
    );
    assert.ok(
      workerRequests > 0,
      "Subagent defaults use the cross-account worker catalog",
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS worker model UI: catalog retry, hidden models, persisted worker selection, rejected stale model, next-turn selection preserves active model, complete lead catalog, shared catalog, responsive layouts. Evidence " +
        root,
    );
  } finally {
    await browser?.close();
  }
});
