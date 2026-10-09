import { readTestState, test, spawnFixture as spawn } from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  modelValue,
  selectModel,
} from "../../../../../runtime/apps/server/tests/model-picker.mjs";
test("Subagent defaults latency @performance", async ({ context }) => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Click-to-Saved latency of the subagent defaults control on the 40-worker
  // fixture with a real second account. No model calls or user state.
  // The target account catalog is delayed by CATALOG_DELAY_MS (default 900,
  // the measured cold catalog time) so the old and the new control face the
  // same server.
  const skill = testRepo;
  const root = await mkdtemp(
    join(tmpdir(), "codex-subagent-defaults-latency-"),
  );
  const catalogDelay = Number(process.env.CATALOG_DELAY_MS ?? 900);
  const models = [
    {
      model: "gpt-6-astra",
      displayName: "Astra",
      levels: ["low", "medium", "high"],
    },
    {
      model: "gpt-5.6-sol",
      displayName: "Sol",
      levels: ["low", "medium", "high"],
    },
    {
      model: "gpt-6-luna",
      displayName: "Luna",
      levels: ["low", "medium", "high"],
    },
    {
      model: "gpt-5.6-luna",
      displayName: "Luna 5.6",
      levels: ["low", "medium", "high"],
    },
  ].map(({ levels, ...row }) => ({
    ...row,
    defaultReasoningEffort: "low",
    supportedReasoningEfforts: levels.map((reasoningEffort) => ({
      reasoningEffort,
    })),
    serviceTiers: [
      { id: "priority", name: "Fast", description: "Faster responses" },
    ],
  }));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(skill, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      },
    },
  );
  let log = "";
  proc.stderr.on("data", (d) => (log += d));
  const timings = {};
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const url = `http://127.0.0.1:${port}`;
  const snapshot = async () => await readTestState(url);
  const post = async (path, body) => {
    const response = await fetch(url + path, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Origin: url,
        "X-Canvas-Token": (await snapshot()).token,
      },
      body: JSON.stringify(body),
    });
    const result = await response.json();
    assert.ok(response.ok, JSON.stringify(result));
    return result;
  };
  const profile = join(root, "second-account");
  await mkdir(profile);
  await writeFile(
    join(profile, "auth.json"),
    JSON.stringify({ OPENAI_API_KEY: "fixture-key-never-sent" }),
  );
  const accounts = await post("/api/accounts/register", { home: profile });
  const second = accounts.accounts.find((a) => a.label === "second-account");
  assert.ok(second);
  const page = await context.newPage();
  await page.exposeFunction("__readTestState", () => readTestState(url));
  await page.setViewportSize({ width: 1440, height: 960 });
  page.setDefaultTimeout(20000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.route("**/api/models?**", async (route) => {
    const key = new URL(route.request().url()).searchParams.get("account_key");
    if (key === second.id && catalogDelay)
      await new Promise((resolve) => setTimeout(resolve, catalogDelay));
    await route.continue();
  });
  page.on("response", (response) => {
    if (response.url().endsWith("/api/agents/account-transfer"))
      timings.transferResponseAt = performance.now();
  });
  page.on("requestfinished", (request) => {
    if (request.url().endsWith("/api/agents/account-transfer"))
      timings.transferRoundTripMs = Math.round(
        (request.timing().responseEnd ?? 0) -
          (request.timing().requestStart ?? 0),
      );
  });
  await page.goto(url);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const lead = (await snapshot()).runtime.agents.find(
    (a) => a.name === "Release lead",
  );
  const openDefaults = async () => {
    const button = page.getByRole("button", {
      name: "Subagent defaults",
      exact: true,
    });
    if (!(await button.isVisible()))
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
    await button.click();
  };
  await openDefaults();
  const dialog = page.getByRole("region", {
    name: "Subagent defaults",
    exact: true,
  });
  await dialog.waitFor();
  const account = dialog.getByLabel(/account/i).first();
  const legacy =
    (await dialog
      .getByRole("button", { name: "Save subagent settings" })
      .count()) > 0 ||
    ((await dialog.locator("select").count()) > 0 &&
      (await dialog
        .getByText("Applies to new subagents. The orchestrator chooses")
        .count()) > 0);
  timings.variant = legacy ? "before" : "after";
  const saved = () => dialog.getByRole("status").filter({ hasText: /^Saved/ });
  const defaultsState = async () =>
    (await snapshot()).runtime.agents.find((a) => a.id === lead.id)
      .workerDefaults;
  // 1. Account change on a 40-worker team.
  const accountStart = performance.now();
  await account.selectOption(second.id);
  timings.accountShownMs = Math.round(performance.now() - accountStart);
  if (legacy) {
    const save = dialog.getByRole("button", {
      name: "Save subagent settings",
      exact: true,
    });
    await page.waitForFunction(
      () => {
        const button = [...document.querySelectorAll("button")].find(
          (node) => node.textContent === "Save subagent settings",
        );
        return button && !button.disabled;
      },
      undefined,
      { timeout: 20000 },
    );
    timings.saveEnabledMs = Math.round(performance.now() - accountStart);
    await save.click();
    await save.waitFor({ state: "hidden" });
    await page.waitForFunction(async (id) => {
      const state = await window.__readTestState();
      return state.runtime.agents.find((a) => a.id === id).workerDefaults
        .accountKey;
    }, lead.id);
  } else {
    await saved().waitFor();
  }
  timings.accountSavedMs = Math.round(performance.now() - accountStart);
  if (timings.transferResponseAt)
    timings.savedAfterResponseMs = Math.round(
      performance.now() - timings.transferResponseAt,
    );
  delete timings.transferResponseAt;
  const stored = await defaultsState();
  assert.equal(stored.accountKey, second.id);
  // Entity sync carries workerDefaults.accountKey and the transfer summary.
  const transferStatus = dialog
    .getByRole("status")
    .filter({ hasText: "moved to second-account" });
  if (legacy) {
    timings.transferStatusShown = await transferStatus
      .waitFor({ timeout: 3000 })
      .then(() => true)
      .catch(() => false);
  } else {
    await transferStatus.waitFor();
    timings.transferStatusMs = Math.round(performance.now() - accountStart);
    timings.transferStatusShown = true;
    assert.equal(
      await dialog.getByRole("button", { name: "Retry", exact: true }).count(),
      0,
    );
  }
  timings.dialogText = (await dialog.innerText())
    .replace(/\s+/g, " ")
    .slice(0, 400);
  for (const width of [390, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await page.waitForTimeout(150);
    await page.screenshot({
      path: join(root, `${timings.variant}-account-${width}.png`),
    });
  }
  // 2. Model change.
  const model = dialog.getByLabel(/model/i).first();
  await page.waitForFunction(() =>
    [...document.querySelectorAll('[role="dialog"] select')].every(
      (node) => !node.disabled,
    ),
  );
  const modelStart = performance.now();
  await selectModel(model, "gpt-5.6-sol");
  timings.modelShownMs = Math.round(performance.now() - modelStart);
  if (legacy) {
    await page.waitForFunction(async (id) => {
      const state = await window.__readTestState();
      return (
        state.runtime.agents.find((a) => a.id === id).workerDefaults.model ===
        "gpt-5.6-sol"
      );
    }, lead.id);
    await page.waitForFunction(() =>
      [...document.querySelectorAll('[role="dialog"] select')].every(
        (node) => !node.disabled,
      ),
    );
  } else {
    await saved().waitFor();
    await page.waitForFunction(async (id) => {
      const state = await window.__readTestState();
      return (
        state.runtime.agents.find((a) => a.id === id).workerDefaults.model ===
        "gpt-5.6-sol"
      );
    }, lead.id);
  }
  timings.modelSavedMs = Math.round(performance.now() - modelStart);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(150);
  await page.screenshot({
    path: join(root, `${timings.variant}-model-1440.png`),
  });
  if (!legacy) {
    // The stored account and the transfer summary survive a reload on the entity path.
    await page.reload();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await openDefaults();
    await dialog.waitFor();
    await page.waitForFunction(
      (id) =>
        [...document.querySelectorAll('[role="dialog"] select')].some(
          (node) => node.value === id,
        ),
      second.id,
    );
    await transferStatus.waitFor();
    assert.equal(
      await modelValue(dialog.getByLabel(/model/i).first()),
      "gpt-5.6-sol",
    );
    assert.equal(
      await dialog
        .getByRole("status")
        .filter({ hasText: /^Moving/ })
        .count(),
      0,
    );
  }
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      catalogDelayMs: catalogDelay,
      ...timings,
      evidence: root,
    }),
  );
});
