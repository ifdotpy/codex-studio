import {
  setupControl,
  setupToggle,
  chooseSetupValue,
} from "../../setup-controls.mjs";
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { modelValue, openModelList, selectModel } from "../../model-picker.mjs";

import { spawnFixture as spawn, test } from "../playwright.mjs";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("execution settings ui", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const skill = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const root = await mkdtemp(join(tmpdir(), "codex-execution-settings-ui-"));
  const models = [
    {
      model: "gpt-6-astra",
      displayName: "Astra",
      description: "Frontier intelligence for the most demanding work.",
      levels: ["low", "medium", "high", "ultra"],
    },
    {
      model: "gpt-5.6-sol",
      displayName: "Sol",
      description: "Latest workhorse model for coding and everyday work.",
      isDefault: true,
      levels: ["low", "medium", "high", "ultra"],
    },
    {
      model: "gpt-6-luna",
      displayName: "Luna",
      description: "Fast and affordable model for easier tasks.",
      levels: ["low", "medium", "high"],
    },
    {
      model: "gpt-5.6-luna",
      displayName: "Luna 5.6",
      description: "Previous generation fast model.",
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
      env: {
        ...process.env,
        EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      },
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
      browser = _browser;
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
    const settingsResponse = (id, matches) =>
      page.waitForResponse((response) => {
        const request = response.request();
        if (
          !response.url().endsWith("/api/conversation") ||
          request.method() !== "POST"
        )
          return false;
        const body = request.postDataJSON();
        return body.id === id && matches(body);
      });
    const waitFor = async (predicate) => {
      for (let i = 0; i < 100; i++) {
        const result = await predicate();
        if (result) return result;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      await page
        .screenshot({ path: join(root, "timeout.png") })
        .catch(() => {});
      throw Error("Timed out: " + predicate.toString() + " " + log);
    };
    const openSettings = async (name) => {
      const settings = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      if (!(await settings.isVisible()))
        await page
          .getByRole("button", { name: "Chat settings", exact: true })
          .click();
      const trigger = settings.locator(".execution-menu");
      if ((await trigger.getAttribute("aria-expanded")) !== "true")
        await trigger.click();
      const role = name === "Subagent defaults" ? "Worker" : "Orchestrator";
      await page.getByRole("button", { name: role, exact: true }).click();
    };
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    const lead = (await state()).find((a) => a.name === "Release lead");
    await openSettings("Main agent settings");
    // Model rows use short names, one recommendation tag, and a selected state.
    const leadModel = page.getByLabel("Main agent model", { exact: true });
    assert.equal(await modelValue(leadModel), lead.model);
    const rows = await openModelList(leadModel);
    assert.deepEqual(
      await rows.evaluateAll((nodes) =>
        nodes.map((node) => [
          node.getAttribute("data-value"),
          node.querySelector(".setup-model-name").textContent,
          !!node.querySelector("small"),
        ]),
      ),
      [
        ["gpt-6-astra", "Astra", false],
        ["gpt-5.6-sol", "Sol", true],
        ["gpt-6-luna", "Luna", false],
        ["gpt-5.6-luna", "Luna 5.6", false],
        ["test-slow", "Standard only", false],
      ],
    );
    assert.equal(
      await page
        .locator(`[role="option"][data-value="${lead.model}"]`)
        .getAttribute("aria-selected"),
      "true",
    );
    await page.screenshot({ path: join(root, "model-picker-open.png") });
    const listBox = page.locator('[role="listbox"]:visible');
    assert.ok(
      await listBox.evaluate(
        (node) => node.getBoundingClientRect().right <= innerWidth,
      ),
      "the model rows fit the window",
    );
    const alternateModelSaved = settingsResponse(
      lead.id,
      (body) => typeof body.model === "string" && body.model !== lead.model,
    );
    await leadModel.focus();
    await leadModel.press("ArrowDown");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("Enter");
    assert.ok(
      (await alternateModelSaved).ok(),
      "keyboard selection is acknowledged",
    );
    await waitFor(async () => {
      const row = (await state()).find((a) => a.id === lead.id);
      return (row.pendingSettings?.model || row.model) !== lead.model;
    });
    await page.keyboard.press("Escape");
    await listBox.waitFor({ state: "hidden" });
    assert.equal(
      await page.evaluate(() =>
        document.activeElement?.classList.contains("execution-menu"),
      ),
      true,
      "Escape returns focus to the picker trigger",
    );
    await openSettings("Main agent settings");
    const originalModelSaved = settingsResponse(
      lead.id,
      (body) => body.model === lead.model,
    );
    await selectModel(leadModel, lead.model);
    assert.ok(
      (await originalModelSaved).ok(),
      "original model save is acknowledged",
    );
    await waitFor(async () => {
      const row = (await state()).find((a) => a.id === lead.id);
      return (row.pendingSettings?.model || row.model) === lead.model;
    });
    assert.equal(await modelValue(leadModel), lead.model);
    const leadReasoning = setupControl(
      page.getByLabel("Main agent reasoning", { exact: true }),
    );
    const leadReasoningSaved = settingsResponse(
      lead.id,
      (body) => body.effort === "high",
    );
    await leadReasoning.selectOption("high");
    assert.ok((await leadReasoningSaved).ok());
    await waitFor(
      async () =>
        (await state()).find((a) => a.id === lead.id).effort === "high",
    );
    await waitFor(() => leadReasoning.isEnabled());
    await page.getByText("Permissions", { exact: true }).click();
    const yolo = page.getByRole("switch", {
      name: "Full access without approval",
      exact: true,
    });
    assert.equal(await yolo.isChecked(), true);
    const yoloSaved = settingsResponse(
      lead.id,
      (body) => body.yolo_mode === false,
    );
    await yolo.click();
    await page
      .locator("#chat-settings-permissions .execution-error[role=alert]")
      .filter({ hasText: "Wait for every team turn" })
      .waitFor();
    assert.equal((await yoloSaved).status(), 400);
    assert.equal((await state()).find((a) => a.id === lead.id).yoloMode, true);
    await waitFor(() => yolo.isChecked());
    await waitFor(() => yolo.isEnabled());
    await openSettings("Main agent settings");
    const leadFastMode = setupToggle(
      page.getByRole("button", { name: "Fast mode", exact: true }),
    );
    const leadFastModeSaved = settingsResponse(
      lead.id,
      (body) => body.fast_mode === true,
    );
    await leadFastMode.check();
    assert.ok((await leadFastModeSaved).ok());
    await waitFor(
      async () =>
        (await state()).find((a) => a.id === lead.id).fastMode === true,
    );
    await waitFor(() => leadFastMode.isEnabled());
    await page.keyboard.press("Escape");
    await page
      .getByRole("dialog", { name: "Main agent settings", exact: true })
      .waitFor({ state: "hidden" });
    assert.ok(
      await page
        .getByRole("dialog", { name: "Chat settings", exact: true })
        .isVisible(),
      "Escape closes only the model detail section",
    );
    await openSettings("Subagent defaults");
    const dialog = page.getByRole("dialog", {
      name: "Subagent defaults",
      exact: true,
    });
    const defaultModel = dialog.getByLabel("Default subagent model", {
      exact: true,
    });
    assert.match(
      await (await openModelList(defaultModel)).first().innerText(),
      /^Same as main agent$/,
    );
    assert.equal(await modelValue(defaultModel), "gpt-6-luna");
    await waitFor(() => defaultModel.isEnabled());
    assert.equal(
      await dialog
        .getByLabel("Default subagent reasoning")
        .locator('input[value="ultra"]')
        .count(),
      0,
    );
    const defaultReasoning = setupControl(
      dialog.getByLabel("Default subagent reasoning"),
    );
    const lowerWorkerReasoningSaved = settingsResponse(
      lead.id,
      (body) => body.worker_defaults?.effort === "low",
    );
    await defaultReasoning.selectOption("low");
    assert.ok((await lowerWorkerReasoningSaved).ok());
    await waitFor(() => defaultReasoning.isEnabled());
    const inheritedReasoningSaved = settingsResponse(
      lead.id,
      (body) => body.worker_defaults?.effort === "high",
    );
    await defaultReasoning.selectOption("high");
    assert.ok((await inheritedReasoningSaved).ok());
    await waitFor(() => defaultReasoning.isEnabled());
    const defaultFastMode = setupToggle(
      dialog.getByRole("button", { name: "Fast mode", exact: true }),
    );
    const inheritedFastModeSaved = settingsResponse(
      lead.id,
      (body) => body.worker_defaults?.fast_mode === true,
    );
    await defaultFastMode.check();
    assert.ok((await inheritedFastModeSaved).ok());
    for (const width of [1440, 390, 320]) {
      await page.setViewportSize({ width, height: 960 });

      assert.ok(
        await dialog.evaluate(
          (node) => node.scrollWidth <= node.clientWidth + 1,
        ),
      );
      await page.screenshot({ path: join(root, `defaults-${width}.png`) });
      // The open model list stays inside the window at every width.
      await openModelList(defaultModel);
      const box = await listBox.boundingBox();
      assert.ok(
        box.x >= 0 && box.x + box.width <= width + 1,
        `model list fits ${width}`,
      );
      await page.screenshot({ path: join(root, `model-picker-${width}.png`) });
    }
    await waitFor(
      async () =>
        !(await dialog.getByLabel("Default subagent model").isDisabled()),
    );
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
    let current = (await state()).find((a) => a.id === lead.id);
    assert.deepEqual(current.workerDefaults, {
      model: "gpt-6-luna",
      effort: "high",
      fastMode: true,
      daybreakEnabled: false,
      cyberAccessProgram: "standard",
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
    assert.equal(first.model, "gpt-6-luna");
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
    assert.equal(await modelValue(defaultModel), "gpt-6-luna");
    const slowModelSaved = settingsResponse(
      lead.id,
      (body) => body.worker_defaults?.model === "test-slow",
    );
    await selectModel(defaultModel, "test-slow");
    assert.ok((await slowModelSaved).ok());
    assert.equal(
      await dialog
        .getByRole("button", { name: "Fast mode", exact: true })
        .count(),
      0,
      "Unsupported modes stay hidden",
    );
    await dialog
      .getByRole("status")
      .filter({ hasText: "Fast mode is unavailable and was turned off" })
      .waitFor();
    assert.equal(
      await dialog
        .getByLabel("Default subagent reasoning")
        .getAttribute("data-value"),
      "low",
    );
    await waitFor(
      async () =>
        (await state()).find((a) => a.id === lead.id).workerDefaults.model ===
        "test-slow",
    );
    await waitFor(() => defaultModel.isEnabled());
    const solModelSaved = settingsResponse(
      lead.id,
      (body) => body.worker_defaults?.model === "gpt-5.6-sol",
    );
    await selectModel(defaultModel, "gpt-5.6-sol");
    assert.ok((await solModelSaved).ok());
    await waitFor(
      async () =>
        (await state()).find((a) => a.id === lead.id).workerDefaults.model ===
        "gpt-5.6-sol",
    );
    await waitFor(() => defaultModel.isEnabled());
    const higherWorkerReasoningSaved = settingsResponse(
      lead.id,
      (body) => body.worker_defaults?.effort === "high",
    );
    await defaultReasoning.selectOption("high");
    assert.ok((await higherWorkerReasoningSaved).ok());
    await waitFor(() => defaultReasoning.isEnabled());
    const lowReasoningSaved = settingsResponse(
      lead.id,
      (body) =>
        body.worker_defaults?.model === "gpt-5.6-sol" &&
        body.worker_defaults?.effort === "low",
    );
    await defaultReasoning.selectOption("low");
    assert.ok((await lowReasoningSaved).ok());
    await waitFor(async () => {
      const defaults = (await state()).find(
        (a) => a.id === lead.id,
      ).workerDefaults;
      return defaults.model === "gpt-5.6-sol" && defaults.effort === "low";
    });
    await waitFor(() => defaultReasoning.isEnabled());
    assert.equal(
      (await dialog
        .getByRole("button", { name: "Fast mode", exact: true })
        .getAttribute("aria-pressed")) === "true",
      false,
    );
    await waitFor(async () => {
      const defaults = (await state()).find(
        (a) => a.id === lead.id,
      ).workerDefaults;
      return (
        defaults.model === "gpt-5.6-sol" &&
        defaults.effort === "low" &&
        defaults.fastMode === false
      );
    });
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
      "gpt-6-luna",
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
    const freshYoloOffSaved = settingsResponse(
      fresh.id,
      (body) => body.yolo_mode === false,
    );
    await freshYolo.uncheck();
    assert.ok((await freshYoloOffSaved).ok());
    await waitFor(
      async () =>
        (await state()).find((a) => a.id === fresh.id).yoloMode === false,
    );
    await waitFor(async () => !(await freshYolo.isDisabled()));
    await waitFor(async () => !(await freshYolo.isChecked()));
    const freshYoloOnSaved = settingsResponse(
      fresh.id,
      (body) => body.yolo_mode === true,
    );
    await freshYolo.check();
    assert.ok((await freshYoloOnSaved).ok());
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
    await chatSettings.waitFor({ state: "hidden" });
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
    await chooseSetupValue(
      page.getByLabel("Subagent reasoning", { exact: true }),
      "medium",
    );
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
      "PASS execution settings UI: short model rows and recommended tags, keyboard list, main agent reasoning/Fast, model choices, saved defaults, child inheritance and overrides, dependent reset notice, immediate save, permission guards, nested Escape, next-turn settings, lost-response replay across reload, active worker unchanged, responsive layout. Evidence " +
        root,
    );
  } finally {
    if (process.env.CODEX_TEST_DESKTOP) await browser?.close();
  }
});
