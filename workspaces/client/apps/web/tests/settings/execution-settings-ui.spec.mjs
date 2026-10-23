import {
  readTestState,
  spawnFixture as spawn,
  rustUiFixture,
  test,
  expect,
} from "../playwright.mjs";
import {
  setupControl,
  setupToggle,
  chooseSetupValue,
} from "../fixtures/setup-controls.mjs";
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  modelValue,
  openModelList,
  selectModel,
} from "../fixtures/model-picker.mjs";

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
  test.setTimeout(60_000);
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
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      FIXTURE_HOLD: "workers",
    },
  });
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
    const state = async () => (await readTestState(url)).runtime.agents;
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
      await expect(settings.getByRole("heading", { level: 3 })).toHaveText([
        "Agents",
        "Team",
        "Access",
      ]);
      const role = name === "Subagent defaults" ? "worker" : "orchestrator";
      const disclosure = settings.locator(`details[data-role="${role}"]`);
      if (!(await disclosure.getAttribute("open"))) {
        // An empty open attribute is still open; use the native property.
        if (!(await disclosure.evaluate((node) => node.open)))
          await disclosure.locator(":scope > summary").click();
      }
      await expect(disclosure).toHaveAttribute("open", "");
    };
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    const lead = (await state()).find((a) => a.name === "Release lead");
    await openSettings("Main agent settings");
    const chatSettingsDialog = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    const parallelInput = chatSettingsDialog.getByRole("textbox", {
      name: "Subagent parallelism",
      exact: true,
    });
    const applyLimit = chatSettingsDialog.locator(".agent-mode-apply");
    await applyLimit.scrollIntoViewIfNeeded();
    const inputBox = await parallelInput.boundingBox();
    const applyBox = await applyLimit.boundingBox();
    expect(applyBox.x).toBeGreaterThan(inputBox.x + inputBox.width);
    expect(
      Math.abs(applyBox.y + applyBox.height - inputBox.y - inputBox.height),
    ).toBeLessThan(1);
    for (const summary of await chatSettingsDialog
      .locator(".execution-disclosure > summary, .execution-accounts > summary")
      .all()) {
      expect(
        await summary.evaluate((node) => getComputedStyle(node).listStyleType),
      ).toBe("none");
      await expect(
        summary.locator(".execution-disclosure-chevron"),
      ).toHaveCount(1);
    }
    if (process.env.EXECUTION_POLISH_SCREENSHOTS) {
      await mkdir(process.env.EXECUTION_POLISH_SCREENSHOTS, {
        recursive: true,
      });
      for (const width of [1440, 390])
        for (const scheme of ["light", "dark"]) {
          await page.setViewportSize({ width, height: 960 });
          await page.emulateMedia({ colorScheme: scheme });
          await expect(page.locator("html")).toHaveAttribute(
            "data-mantine-color-scheme",
            scheme,
          );
          for (const opened of await chatSettingsDialog
            .locator("details[open]")
            .all())
            await opened.locator(":scope > summary").click();
          for (const scene of ["overview", "worker", "accounts"]) {
            if (scene === "worker")
              await chatSettingsDialog
                .locator('details[data-role="worker"] > summary')
                .click();
            if (scene === "accounts") {
              await chatSettingsDialog
                .locator('details[data-role="worker"] > summary')
                .click();
              await chatSettingsDialog
                .locator(".execution-accounts > summary")
                .click();
              await chatSettingsDialog
                .locator(".execution-accounts")
                .scrollIntoViewIfNeeded();
            } else
              await chatSettingsDialog.evaluate((node) => {
                for (let el = node; el; el = el.parentElement) el.scrollTop = 0;
              });
            expect(
              await chatSettingsDialog.evaluate(
                (node) => node.scrollWidth <= node.clientWidth,
              ),
            ).toBe(true);
            await page.screenshot({
              path: join(
                process.env.EXECUTION_POLISH_SCREENSHOTS,
                `polish-chat-settings-${scene}-${width}-${scheme}.png`,
              ),
            });
          }
        }
      await page.setViewportSize({ width: 1440, height: 960 });
      await page.emulateMedia({ colorScheme: "light" });
      await openSettings("Main agent settings");
    }
    // Model rows use short names, one recommendation tag, and a selected state.
    const leadModel = page.getByLabel("Main agent model", { exact: true });
    assert.equal(await modelValue(leadModel), lead.model);
    const rows = await openModelList(leadModel);
    assert.deepEqual(
      await rows.evaluateAll((nodes) =>
        nodes.map((node) => [
          node.getAttribute("data-value"),
          node.labels[0].textContent,
          node.labels[0].textContent.includes("(default)"),
        ]),
      ),
      [
        ["gpt-6-astra", "Astra 6", false],
        ["gpt-5.6-sol", "Sol 5.6 (default)", true],
        ["gpt-6-luna", "Luna 6", false],
        ["gpt-5.6-luna", "Luna 5.6", false],
        ["test-slow", "Standard only", false],
      ],
    );
    await expect(
      leadModel.locator(`input[type="radio"][data-value="${lead.model}"]`),
    ).toBeChecked();
    const listBox = leadModel;
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
    await leadModel.getByRole("radio", { checked: true }).focus();
    await page.keyboard.press("ArrowDown");
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
    await expect(
      page.getByRole("button", { name: "Chat settings", exact: true }),
    ).toBeFocused();
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
    await waitFor(async () => (await modelValue(leadModel)) === lead.model);
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
    const yolo = page.getByRole("radio", {
      name: "Full",
      exact: true,
    });
    assert.equal(await yolo.isChecked(), true);
    const yoloSaved = settingsResponse(
      lead.id,
      (body) => body.yolo_mode === false,
    );
    await page
      .locator("#chat-settings-permissions")
      .getByText("Ask", { exact: true })
      .click();
    await page
      .locator("#chat-settings-permissions .execution-error[role=alert]")
      .filter({ hasText: "Wait for this turn to end" })
      .waitFor();
    assert.equal((await yoloSaved).status(), 400);
    assert.equal((await state()).find((a) => a.id === lead.id).yoloMode, true);
    await waitFor(() => yolo.isChecked());
    await waitFor(() => yolo.isEnabled());
    await openSettings("Main agent settings");
    const leadFastMode = setupToggle(
      page.getByRole("switch", { name: "Fast mode", exact: true }),
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
    await expect(
      page.getByRole("dialog", { name: "Chat settings", exact: true }),
    ).not.toBeVisible();
    await openSettings("Subagent defaults");
    const dialog = page.locator('details[data-role="worker"]');
    await page.locator('details[data-role="review"] > summary').click();
    const reviewDialog = page.locator('details[data-role="review"]');
    await expect(
      reviewDialog.getByRole("radiogroup", {
        name: "Default review model",
        exact: true,
      }),
    ).toBeVisible();
    await dialog.locator(":scope > summary").click();
    await expect(dialog).toHaveAttribute("open", "");
    const delegationDepth = page.getByRole("combobox", {
      name: "Delegation depth",
    });
    assert.equal(await delegationDepth.inputValue(), "1");
    assert.equal(
      await page
        .getByText(
          "Lowering blocks new delegation immediately. Existing descendants keep running.",
        )
        .isVisible(),
      true,
    );
    const nestedDelegationSaved = settingsResponse(
      lead.id,
      (body) => body.worker_defaults?.delegation_depth === 2,
    );
    await delegationDepth.selectOption("2");
    assert.ok((await nestedDelegationSaved).ok());
    await waitFor(
      async () =>
        (await state()).find((a) => a.id === lead.id).workerDefaults
          .delegationDepth === 2,
    );
    const defaultModel = dialog.getByLabel("Default subagent model", {
      exact: true,
    });
    assert.match(
      await (
        await openModelList(defaultModel)
      )
        .first()
        .evaluate((node) => node.labels[0].textContent),
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
      dialog.getByRole("switch", { name: "Fast mode", exact: true }),
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
      // The open model list stays inside the window at every width.
      await openModelList(defaultModel);
      const box = await defaultModel.boundingBox();
      assert.ok(
        box.x >= 0 && box.x + box.width <= width + 1,
        `model list fits ${width}`,
      );
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
      delegationDepth: 2,
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
    await dialog
      .getByRole("switch", { name: "Fast mode", exact: true })
      .waitFor({ state: "hidden" });
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
    const highReasoningOption = dialog
      .getByRole("radiogroup", {
        name: "Default subagent reasoning",
        exact: true,
      })
      .getByText("High", { exact: true });
    await highReasoningOption.waitFor({ state: "visible" });
    await highReasoningOption.click();
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
    await expect(
      dialog.getByRole("switch", { name: "Fast mode", exact: true }),
    ).not.toBeChecked();
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
    const snapshot = await readTestState(url);
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
    const freshYolo = page.getByRole("radio", {
      name: "Full",
      exact: true,
    });
    assert.equal(await freshYolo.isChecked(), true);
    const freshYoloOffSaved = settingsResponse(
      fresh.id,
      (body) => body.yolo_mode === false,
    );
    await page
      .locator("#chat-settings-permissions")
      .getByText("Ask", { exact: true })
      .click();
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
    await page
      .locator("#chat-settings-permissions")
      .getByText("Full", { exact: true })
      .click();
    assert.ok((await freshYoloOnSaved).ok());
    await waitFor(
      async () =>
        (await state()).find((a) => a.id === fresh.id).yoloMode === true,
    );
    await waitFor(async () => !(await freshYolo.isDisabled()));
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
  } catch (error) {
    throw new Error(`${error.message}\nFixture log:\n${log}`, { cause: error });
  } finally {
    if (process.env.CODEX_TEST_DESKTOP) await browser?.close();
  }
});
