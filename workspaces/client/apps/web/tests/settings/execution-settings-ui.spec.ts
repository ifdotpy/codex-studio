import { required } from "./fixture-helpers.ts";
import type {
  Browser,
  ElectronApplication,
  Page,
  TestInfo,
  BrowserContext,
} from "@playwright/test";
import { _electron } from "playwright";
import type { PostBody } from "../../src/api.ts";
import {
  readTestState,
  spawnFixture as spawn,
  rustUiFixture,
  test,
  expect,
} from "../playwright.ts";
import {
  setupControl,
  setupToggle,
  chooseSetupValue,
} from "../fixtures/setup-controls.ts";
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  modelValue,
  openModelList,
  selectModel,
} from "../fixtures/model-picker.ts";

const browserContextsByTest = new WeakMap<TestInfo, Set<BrowserContext>>();
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
  let browser: Browser | ElectronApplication | undefined;
  let log = "";
  proc.stderr?.on("data", (d) => (log += d));
  try {
    const port = await new Promise<number>((resolve, reject) => {
      proc.stdout?.once("data", (d) => resolve(Number(String(d).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const url = `http://127.0.0.1:${port}`;
    let page: Page;
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
        await browser.evaluate(({ BrowserWindow }) => {
          const first = BrowserWindow.getAllWindows()[0];
          if (!first) throw new Error("Missing hidden fixture window");
          return first.isVisible();
        }),
        false,
      );
    } else {
      browser = _browser;
      page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
    }
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    if (process.env.CODEX_TEST_DESKTOP) {
      await page.waitForURL(url + "/");
      await page.locator("#message").waitFor();
    } else {
      await page.goto(url);
    }
    const state = async () =>
      (await readTestState(url, { agentDetails: true })).runtime.agents;
    const settingsResponse = (
      id: string,
      matches: (body: PostBody<"/api/conversation">) => boolean,
    ) =>
      page.waitForResponse((response) => {
        const request = response.request();
        if (
          !response.url().endsWith("/api/conversation") ||
          request.method() !== "POST"
        )
          return false;
        const body: PostBody<"/api/conversation"> = request.postDataJSON();
        return body.id === id && matches(body);
      });
    const waitFor = async <T>(predicate: () => T | Promise<T>) => {
      for (let i = 0; i < 100; i++) {
        const result = await predicate();
        if (result) return result;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      throw Error("Timed out: " + predicate.toString() + " " + log);
    };
    const picker = page.locator(".execution-dropdown");
    const chatSettingsDialog = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    const openTeam = async () => {
      if (await picker.isVisible()) {
        await page.keyboard.press("Escape");
        await expect(picker).toBeHidden();
      }
      if (!(await chatSettingsDialog.isVisible())) {
        const toggle = page.locator("#chat-settings-toggle");
        if (await toggle.isVisible()) await toggle.click();
        else {
          await page
            .getByRole("button", { name: "Chat actions", exact: true })
            .click();
          await page
            .getByRole("menuitem", { name: "Chat settings", exact: true })
            .click();
        }
      }
      await expect(chatSettingsDialog.getByRole("tablist")).toHaveCount(0);
      await expect(chatSettingsDialog.getByRole("listbox")).toHaveCount(0);
    };
    const openSettings = async (name: string) => {
      if (await chatSettingsDialog.isVisible()) {
        await page.keyboard.press("Escape");
        await expect(chatSettingsDialog).toBeHidden();
      }
      if (!(await picker.isVisible()))
        await page.locator("#composer .execution-summary-trigger").click();
      const role =
        name === "Subagent defaults" || name === "Subagent settings"
          ? "Workers"
          : "Lead";
      for (const roleName of ["Lead", "Workers", "Review"])
        await expect(
          picker.getByRole("radio", { name: roleName, exact: true }),
        ).toHaveCount(1);
      await picker
        .getByRole("radiogroup", { name: "Agent role", exact: true })
        .getByText(role, { exact: true })
        .click();
    };
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    const lead = (await state()).find((a) => a.name === "Release lead");
    assert.ok(lead);
    assert.ok(lead.model);
    assert.ok(lead.concurrency != null);
    await page.locator("#message").fill("/model");
    await page.locator("#message").press("Enter");
    await expect(picker).toBeVisible();
    await expect(picker).toHaveClass(/mantine-Popover-dropdown/);
    await expect(chatSettingsDialog).toBeHidden();
    await expect(page.locator("#message")).toHaveValue("");
    await page.keyboard.press("Escape");
    await expect(picker).toBeHidden();
    await expect(
      page.locator("#composer .execution-summary-trigger"),
    ).toBeFocused();
    await openTeam();
    const parallelInput = chatSettingsDialog.getByRole("textbox", {
      name: "Subagent parallelism",
      exact: true,
    });
    const applyLimit = chatSettingsDialog.locator(".agent-mode-apply");
    await expect(applyLimit).toHaveCount(0);
    await parallelInput.fill(String(lead.concurrency + 1));
    await applyLimit.scrollIntoViewIfNeeded();
    const inputBox = await parallelInput.boundingBox();
    const applyBox = await applyLimit.boundingBox();
    expect(required(applyBox).x).toBeGreaterThan(
      required(inputBox).x + required(inputBox).width,
    );
    expect(
      Math.abs(
        required(applyBox).y +
          required(applyBox).height -
          required(inputBox).y -
          required(inputBox).height,
      ),
    ).toBeLessThan(1);
    await parallelInput.fill(String(lead.concurrency));
    await expect(applyLimit).toHaveCount(0);
    const screenshotDirectory = process.env.EXECUTION_COMPACT_SCREENSHOTS;
    if (screenshotDirectory)
      await mkdir(screenshotDirectory, { recursive: true });
    const layoutMeasurements = [];
    for (const [width, height] of [
      [1440, 900],
      [390, 844],
    ] as const) {
      for (const scheme of ["light", "dark"] as const) {
        await page.setViewportSize({ width, height });
        await page.emulateMedia({ colorScheme: scheme });
        await expect(page.locator("html")).toHaveAttribute(
          "data-mantine-color-scheme",
          scheme,
        );
        for (const surface of ["composer-picker", "compact-team"]) {
          if (surface === "composer-picker")
            await openSettings("Main agent settings");
          else await openTeam();
          const visible =
            surface === "composer-picker" ? picker : chatSettingsDialog;
          await expect(visible).toBeVisible();
          const geometry = await visible.evaluate((node) => {
            const bounds = node.getBoundingClientRect();
            return {
              left: bounds.left,
              right: bounds.right,
              top: bounds.top,
              bottom: bounds.bottom,
              content: node.scrollWidth,
              width: node.clientWidth,
            };
          });
          expect(geometry.left).toBeGreaterThanOrEqual(0);
          expect(geometry.right).toBeLessThanOrEqual(width);
          expect(geometry.content).toBeLessThanOrEqual(geometry.width);
          layoutMeasurements.push({
            viewportWidth: width,
            height,
            scheme,
            surface,
            ...geometry,
          });
          if (screenshotDirectory)
            await page.screenshot({
              animations: "disabled",
              path: join(
                screenshotDirectory,
                `${surface}-${width}-${scheme}.png`,
              ),
            });
        }
        if (screenshotDirectory)
          await writeFile(
            join(screenshotDirectory, "layout-measurements.json"),
            JSON.stringify(layoutMeasurements, null, 2),
          );
      }
    }
    await page.setViewportSize({ width: 1440, height: 960 });
    await page.emulateMedia({ colorScheme: "light" });
    await openSettings("Main agent settings");
    // Model rows use short names, one recommendation tag, and a selected state.
    const leadModel = page.getByLabel("Main agent model", { exact: true });
    assert.equal(await modelValue(leadModel), lead.model);
    const rows = await openModelList(leadModel);
    assert.deepEqual(
      await rows.evaluateAll((nodes) =>
        nodes.map((node) => [
          node.getAttribute("data-value"),
          node.textContent,
          node.textContent.includes("recommended"),
        ]),
      ),
      [
        ["gpt-6-astra", "Astra 6", false],
        ["gpt-5.6-sol", "Sol 5.6recommended", true],
        ["gpt-6-luna", "Luna 6", false],
        ["gpt-5.6-luna", "Luna 5.6", false],
        ["test-slow", "Standard only", false],
      ],
    );
    await expect(
      leadModel.locator(`[role="option"][data-value="${lead.model}"]`),
    ).toHaveAttribute("aria-selected", "true");
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
    await leadModel.getByRole("option", { selected: true }).focus();
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("Enter");
    assert.ok(
      (await alternateModelSaved).ok(),
      "keyboard selection is acknowledged",
    );
    await waitFor(async () => {
      const row = (await state()).find((a) => a.id === lead.id);
      return (
        (required(row).pendingSettings?.model || required(row).model) !==
        lead.model
      );
    });
    await page.keyboard.press("Escape");
    await listBox.waitFor({ state: "hidden" });
    await expect(
      page.locator("#composer .execution-summary-trigger"),
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
      return (
        (required(row).pendingSettings?.model || required(row).model) ===
        lead.model
      );
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
        required((await state()).find((a) => a.id === lead.id)).effort ===
        "high",
    );
    await waitFor(() => leadReasoning.isEnabled());
    await openTeam();
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
    assert.equal(
      required((await state()).find((a) => a.id === lead.id)).yoloMode,
      true,
    );
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
        required((await state()).find((a) => a.id === lead.id)).fastMode ===
        true,
    );
    await waitFor(() => leadFastMode.isEnabled());
    await page.keyboard.press("Escape");
    await expect(picker).toBeHidden();
    await openSettings("Subagent defaults");
    const dialog = picker;
    await picker
      .getByRole("radiogroup", { name: "Agent role", exact: true })
      .getByText("Review", { exact: true })
      .click();
    const reviewDialog = picker;
    await expect(
      reviewDialog.getByRole("listbox", {
        name: "Default review model",
        exact: true,
      }),
    ).toBeVisible();
    await openTeam();
    const delegationDepth = page.getByRole("combobox", {
      name: "Delegation depth",
    });
    assert.equal(await delegationDepth.inputValue(), "1");
    assert.equal(
      await page.getByText("Existing workers keep their depth.").isVisible(),
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
        required(
          required((await state()).find((a) => a.id === lead.id))
            .workerDefaults,
        ).delegationDepth === 2,
    );
    await openSettings("Subagent defaults");
    const defaultModel = dialog.getByLabel("Default subagent model", {
      exact: true,
    });
    assert.match(
      await (
        await openModelList(defaultModel)
      )
        .first()
        .evaluate((node) => node.textContent),
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
      // The open model list stays inside the window at every width. A width
      // change remounts the responsive sheet; measure after it is visible.
      await openModelList(defaultModel);
      await defaultModel.waitFor({ state: "visible" });
      const box = await defaultModel.boundingBox();
      assert.ok(
        required(box).x >= 0 &&
          required(box).x + required(box).width <= width + 1,
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
    assert.deepEqual(required(current).workerDefaults, {
      model: "gpt-6-luna",
      effort: "high",
      fastMode: true,
      daybreakEnabled: false,
      cyberAccessProgram: "standard",
      delegationDepth: 2,
    });
    const spawnWorker = async (
      name: string,
      overrides: Partial<PostBody<"/api/agents">> = {},
    ) => {
      proc.stdin?.write(
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
        required(
          required((await state()).find((a) => a.id === lead.id))
            .workerDefaults,
        ).model === "test-slow",
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
        required(
          required((await state()).find((a) => a.id === lead.id))
            .workerDefaults,
        ).model === "gpt-5.6-sol",
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
    await expect(defaultReasoning.locator('input[value="high"]')).toBeChecked();
    const lowReasoningSaved = settingsResponse(
      lead.id,
      (body) =>
        body.worker_defaults?.model === "gpt-5.6-sol" &&
        body.worker_defaults?.effort === "low",
    );
    await defaultReasoning.selectOption("low");
    assert.ok((await lowReasoningSaved).ok());
    await waitFor(async () => {
      const defaults = required(
        (await state()).find((a) => a.id === lead.id),
      ).workerDefaults;
      return (
        required(defaults).model === "gpt-5.6-sol" &&
        required(defaults).effort === "low"
      );
    });
    await waitFor(() => defaultReasoning.isEnabled());
    await expect(
      dialog.getByRole("switch", { name: "Fast mode", exact: true }),
    ).not.toBeChecked();
    await waitFor(async () => {
      const defaults = required(
        (await state()).find((a) => a.id === lead.id),
      ).workerDefaults;
      return (
        required(defaults).model === "gpt-5.6-sol" &&
        required(defaults).effort === "low" &&
        required(defaults).fastMode === false
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
      required((await state()).find((a) => a.id === first.id)).model,
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
    const fresh: import("../../src/types.ts").Agent =
      await freshResponse.json();
    await page.reload();
    await page.locator(`[data-chat="${fresh.id}"]`).click();
    await openSettings("Main agent settings");
    await openTeam();
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
        required((await state()).find((a) => a.id === fresh.id)).yoloMode ===
        false,
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
        required((await state()).find((a) => a.id === fresh.id)).yoloMode ===
        true,
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
      const row = page.locator(`[data-worker="${required(running).id}"]`);
      if (!(await row.isVisible()))
        await page.getByRole("button", { name: "Team", exact: true }).click();
      await row.click();
    };
    await openWorker();
    await openSettings("Subagent settings");
    const workerDialog = picker;
    for (const roleName of ["Lead", "Review"]) {
      await workerDialog.getByText(roleName, { exact: true }).click();
      await expect(
        workerDialog.getByText(
          "Inherited from the lead. Open the lead chat to change these settings.",
        ),
      ).toBeVisible();
      const models = workerDialog.getByRole("listbox");
      await expect(models).toHaveCount(1);
      await expect(
        models.locator('[role="option"]:not(:disabled)'),
      ).toHaveCount(0);
    }
    await workerDialog.getByText("Workers", { exact: true }).click();
    await expect(
      workerDialog.getByLabel("Subagent reasoning", { exact: true }),
    ).toBeVisible();
    const pendingBodies: PostBody<"/api/conversation">[] = [];
    await page.route("**/api/conversation", async (route) => {
      const body: PostBody<"/api/conversation"> = route
        .request()
        .postDataJSON();
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
      required(
        (await state()).find((agent) => agent.id === required(running).id),
      ).effort,
      required(running).effort,
      "The active turn keeps its reasoning",
    );
    assert.equal(
      required(
        required(
          (await state()).find((agent) => agent.id === required(running).id),
        ).pendingSettings,
      ).effort,
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
    assert.match(
      required(required(pendingBodies[0]).request_id),
      /^[0-9a-f-]{36}$/,
    );
    assert.equal(
      required(
        (await state()).find((agent) => agent.id === required(running).id),
      ).effort,
      required(running).effort,
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS execution settings UI: short model rows and recommended tags, keyboard list, main agent reasoning/Fast, model choices, saved defaults, child inheritance and overrides, dependent reset notice, immediate save, permission guards, nested Escape, next-turn settings, lost-response replay across reload, active worker unchanged, responsive layout. Evidence " +
        root,
    );
  } catch (error) {
    throw new Error(
      `${error instanceof Error ? error.message : String(error)}\nFixture log:\n${log}`,
      { cause: error },
    );
  } finally {
    if (process.env.CODEX_TEST_DESKTOP) await browser?.close();
  }
});
