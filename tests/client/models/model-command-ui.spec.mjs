import { chooseSetupValue } from "../../setup-controls.mjs";
// Production renderer, isolated runtime, real settings and message endpoints.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { modelValue, selectModel } from "../../model-picker.mjs";

import { test, expect } from "../playwright.mjs";
import { spawnFixture as spawn } from "../playwright.mjs";

test("model-command-ui", async ({ browser }) => {
  test.setTimeout(120_000);
  const repo = join(import.meta.dirname, "../../../");
  const root = await mkdtemp(join(tmpdir(), "studio-model-command-ui-"));
  const models = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"].map(
    (model) => ({
      model,
      defaultReasoningEffort: "medium",
      supportedReasoningEfforts: (model === "gpt-5.6-luna"
        ? ["low", "medium", "high", "max"]
        : ["low", "medium", "high", "ultra"]
      ).map((reasoningEffort) => ({ reasoningEffort })),
      serviceTiers: [{ id: "priority" }],
    }),
  );
  const proc = spawn(
    process.env.PYTHON || "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        CODEX_BOARD_STATE_DIR: join(root, "board"),
        EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      },
    },
  );
  let context,
    page,
    log = "";
  proc.stderr.on("data", (data) => {
    log += data;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(Error("Fixture startup timed out: " + log)),
        30000,
      );
      proc.stdout.once("data", (data) => {
        clearTimeout(timer);
        resolve(Number(String(data).trim()));
      });
      proc.once("exit", () => {
        clearTimeout(timer);
        reject(Error(log || "Fixture exited"));
      });
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = () =>
      fetch(origin + "/api/state").then((response) => response.json());
    const initial = await snapshot();
    const main = initial.threads.find(
      (agent) => agent.name === "Other project",
    );
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const worker = initial.threads.find((agent) => agent.name === "Worker 00");
    const agent = async (id) =>
      (await snapshot()).runtime.agents.find((value) => value.id === id);
    // The API projection below is deliberately stale in this scenario; read
    // the fixture's persisted row independently instead of extending its wait.
    const persistedAgent = (id) => {
      const query = `import json, sqlite3, sys
db = sqlite3.connect("file:" + sys.argv[1] + "?mode=ro", uri=True, timeout=2)
row = db.execute("SELECT record FROM runtime_agents WHERE id = ?", (sys.argv[2],)).fetchone()
if row is None:
    raise SystemExit("fixture agent row is missing")
record = json.loads(row[0])
print(json.dumps({key: record.get(key) for key in ("id", "model", "effort", "pendingSettings")}))
db.close()`;
      const result = spawnSync(
        process.env.PYTHON || "python3",
        ["-c", query, join(root, "canvas.sqlite3"), id],
        { encoding: "utf8", timeout: 4000 },
      );
      assert.equal(result.status, 0, result.stderr || result.error?.message);
      return JSON.parse(result.stdout);
    };
    const wait = async (test, label) => {
      for (let i = 0; i < 100; i++) {
        if (await test()) return;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      throw Error(
        `${label}\nMain: ${JSON.stringify(await agent(main.id))}\nWorker: ${JSON.stringify(await agent(worker.id))}\nSettings: ${JSON.stringify(settings)}\nPage errors: ${JSON.stringify(errors)}\n${log}`,
      );
    };
    context = await browser.newContext({
      viewport: { width: 1440, height: 960 },
    });
    page = await context.newPage();
    page.setDefaultTimeout(10000);
    const errors = [],
      settings = [],
      messages = [];
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("request", (request) => {
      if (request.method() !== "POST") return;
      const path = new URL(request.url()).pathname;
      if (path === "/api/conversation") settings.push(request.postDataJSON());
      if (path === "/api/messages") messages.push(request.postDataJSON());
    });
    let lagMainSettings = false;
    let laggedProjections = 0;
    const staleEntityVersions = [];
    await page.route("**/api/sync/pull?*", async (route) => {
      const scope = new URL(route.request().url()).searchParams.get("scope");
      if (
        !lagMainSettings ||
        !["state", "state:chat", "state:entities:v1"].includes(scope)
      )
        return route.continue();
      const response = await route.fetch();
      const data = await response.json();
      let staleRows = 0;
      for (const document of data.documents || []) {
        const payload = JSON.parse(document.payload);
        const values =
          payload.collection === "agent"
            ? [payload.value]
            : [...(payload.threads || []), ...(payload.runtime?.agents || [])];
        let changedMain = false;
        for (const value of values) {
          if (value.id === main.id) {
            staleEntityVersions.push({
              scope,
              id: document.id,
              seq: document.seq,
              returnedModel: value.model,
              returnedEffort: value.effort,
              staleModel: main.model,
              staleEffort: main.effort,
            });
            value.model = main.model;
            value.effort = main.effort;
            changedMain = true;
          }
        }
        if (changedMain) {
          document.payload = JSON.stringify(payload);
          staleRows++;
        }
      }
      await route.fulfill({ response, json: data });
      laggedProjections += staleRows;
    });
    await page.addInitScript(
      ({ id, stateDir }) => {
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        );
        localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
      },
      { id: main.id, stateDir: initial.stateDir },
    );
    await page.goto(origin);
    await page.locator(`[data-chat="${main.id}"]`).click();
    const composer = page.locator("#message");
    const dialog = (worker = false) =>
      page.getByRole("dialog", {
        name: worker ? "Subagent settings" : "Main agent settings",
        exact: true,
      });
    const saved = async (worker = false) =>
      expect(dialog(worker).locator(".execution-status")).toHaveText(
        /^Saved(?: ·.*)?$/,
      );
    let openedBySend = false;
    const open = async (text = "/model", send = false, worker = false) => {
      openedBySend = send;
      await composer.fill(text);
      await expect(
        page.getByText("No matching skills", { exact: true }),
      ).toHaveCount(0);
      if (send) await page.locator("#send").click();
      else await composer.press("Enter");
      await dialog(worker).waitFor();
    };
    const focusChecks = [];
    const close = async (worker = false) => {
      await page.keyboard.press("Escape");
      await dialog(worker).waitFor({ state: "hidden" });
      const focused = await page
        .waitForFunction(
          (allowSend) =>
            document.activeElement?.id === "message" ||
            (allowSend && document.activeElement?.id === "send"),
          openedBySend,
          { timeout: 1000 },
        )
        .then(
          () => true,
          () => false,
        );
      focusChecks.push(focused);
      if (!focused)
        console.error(
          "Escape focus:",
          await page.evaluate(() => ({
            tag: document.activeElement?.tagName,
            id: document.activeElement?.id,
            label: document.activeElement?.getAttribute("aria-label"),
            text: document.activeElement?.textContent?.slice(0, 60),
          })),
        );
    };
    await page.getByLabel("Choose attachments").setInputFiles({
      name: "model-context.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("Keep this context when selecting a model."),
    });
    const attachment = page.getByRole("button", {
      name: "Remove model-context.txt",
      exact: true,
    });
    await attachment.waitFor();
    await open("  /MoDeL  ");
    assert.equal(messages.length, 0);
    assert.equal(settings.length, 0);
    await close();
    assert.equal(settings.length, 0);
    await attachment.waitFor();
    console.log(
      "PASS case-insensitive standalone command; Escape changes no settings; attachment retained",
    );
    await open("/model", true);
    await selectModel(
      dialog().getByLabel("Main agent model", { exact: true }),
      "gpt-5.6-sol",
    );
    await saved();
    const mainAfterModel = persistedAgent(main.id);
    assert.deepEqual(
      { model: mainAfterModel.model, effort: mainAfterModel.effort },
      { model: "gpt-5.6-sol", effort: "medium" },
      "The main model is persisted in the isolated fixture",
    );
    await chooseSetupValue(
      dialog().getByLabel("Main agent reasoning", { exact: true }),
      "high",
    );
    await saved();
    const mainAfterReasoning = persistedAgent(main.id);
    assert.deepEqual(
      { model: mainAfterReasoning.model, effort: mainAfterReasoning.effort },
      { model: "gpt-5.6-sol", effort: "high" },
      "Main reasoning is persisted in the isolated fixture",
    );
    lagMainSettings = true;
    await wait(
      () => laggedProjections > 0,
      "The replica returned stale main settings",
    );
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    const afterStaleSync = persistedAgent(main.id);
    assert.deepEqual(
      { model: afterStaleSync.model, effort: afterStaleSync.effort },
      { model: "gpt-5.6-sol", effort: "high" },
      "A stale sync projection cannot overwrite persisted settings",
    );
    // Restrict the stale-replica behavior to this assertion; later deliberate
    // settings changes must reach the real fixture without stale pulls racing them.
    lagMainSettings = false;
    assert.equal(messages.length, 0);
    assert(
      settings
        .filter((body) => body.id === main.id)
        .every(
          (body) =>
            body.expected_account_key === (main.accountKey || "default"),
        ),
    );
    await page.screenshot({
      path: join(root, "desktop-picker.png"),
      animations: "disabled",
    });
    await close();
    await attachment.waitFor();
    await composer.fill("/model gpt-5.6-sol");
    await composer.press("Enter");
    await page
      .getByText(/Type \/model|Use \/model|Enter \/model/)
      .first()
      .waitFor();
    assert.equal(messages.length, 0);
    assert.equal(await dialog().isVisible(), false);
    await composer.fill("/model");
    await page
      .getByRole("button", { name: "Choose model with /model", exact: true })
      .click();
    await dialog().waitFor();
    await wait(
      () =>
        dialog().getByLabel("Main agent model", { exact: true }).isEnabled(),
      "Reopened catalog ready",
    );
    const reopened = await modelValue(
      dialog().getByLabel("Main agent model", { exact: true }),
    );
    assert.equal(
      await dialog()
        .getByLabel("Main agent reasoning", { exact: true })
        .getAttribute("data-value"),
      "high",
      "The stale sync response does not replace saved reasoning",
    );
    if (reopened !== "gpt-5.6-sol")
      console.error("Reopened model:", {
        shown: reopened,
        stored: (await agent(main.id)).model,
        requests: settings
          .filter((body) => body.id === main.id)
          .map(({ model, effort, next_turn }) => ({
            model,
            effort,
            next_turn,
          })),
      });
    await saved();
    assert.equal(
      reopened,
      "gpt-5.6-sol",
      "Reopened picker uses the saved main model",
    );
    assert(
      laggedProjections > 0,
      "The fixture exercised a lagging settings replica",
    );
    lagMainSettings = false;
    const beforeSwitch = settings.length;
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await dialog().waitFor({ state: "hidden" });
    assert.equal(
      settings.length,
      beforeSwitch,
      "A chat switch closes the picker without a settings write",
    );
    console.log(
      "PASS Send and suggestion open the picker; real main settings persist; arguments never become messages",
    );
    await page.locator(`[data-chat="${lead.id}"]`).click();
    const workerRow = page.locator(`[data-worker="${worker.id}"]`);
    if (!(await workerRow.isVisible()))
      await page.locator("#team-toggle").click();
    await workerRow.click();
    await open("/model", false, true);
    await selectModel(
      dialog(true).getByLabel("Subagent model", { exact: true }),
      "gpt-5.6-sol",
    );
    await saved(true);
    const workerAfterModel = persistedAgent(worker.id);
    assert.deepEqual(
      {
        model: workerAfterModel.model,
        effort: workerAfterModel.effort,
        pendingModel: workerAfterModel.pendingSettings?.model,
        pendingEffort: workerAfterModel.pendingSettings?.effort,
      },
      {
        model: worker.model,
        effort: worker.effort,
        pendingModel: "gpt-5.6-sol",
        pendingEffort: "high",
      },
      "Active worker's next-turn model is persisted",
    );
    await dialog(true)
      .getByLabel("Subagent reasoning", { exact: true })
      .selectOption("medium");
    await saved(true);
    const running = persistedAgent(worker.id);
    assert.deepEqual(
      {
        model: running.model,
        effort: running.effort,
        pendingModel: running.pendingSettings?.model,
        pendingEffort: running.pendingSettings?.effort,
      },
      {
        model: worker.model,
        effort: worker.effort,
        pendingModel: "gpt-5.6-sol",
        pendingEffort: "medium",
      },
      "Active worker's next-turn reasoning is persisted",
    );
    assert(
      settings
        .filter((body) => body.id === worker.id)
        .every(
          (body) =>
            body.next_turn === true &&
            body.expected_account_key === (worker.accountKey || "default"),
        ),
    );
    assert.equal(messages.length, 0);
    await close(true);
    console.log(
      "PASS active subagent uses next-turn settings and retains its current model",
    );
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForFunction(
      () =>
        document.querySelector("#conversation-title")?.textContent ===
        "Other project",
    );
    await open("/model", true);
    await wait(
      () =>
        dialog().getByLabel("Main agent model", { exact: true }).isEnabled(),
      "Mobile catalog ready",
    );
    assert.equal(
      await modelValue(
        dialog().getByLabel("Main agent model", { exact: true }),
      ),
      "gpt-5.6-sol",
      "Mobile picker retains the saved main model",
    );
    await chooseSetupValue(
      dialog().getByLabel("Main agent reasoning", { exact: true }),
      "medium",
    );
    await saved();
    const mainAfterMobileReasoning = persistedAgent(main.id);
    assert.deepEqual(
      {
        model: mainAfterMobileReasoning.model,
        effort: mainAfterMobileReasoning.effort,
      },
      { model: "gpt-5.6-sol", effort: "medium" },
      "Mobile reasoning is persisted in the isolated fixture",
    );
    assert.equal(
      await dialog().evaluate(
        (node) => node.scrollWidth <= node.clientWidth + 1,
      ),
      true,
      "Picker has no horizontal overflow",
    );
    const bounds = await dialog().boundingBox();
    assert(
      bounds.x >= 0 && bounds.x + bounds.width <= 391,
      "Picker fits 390px viewport",
    );
    await page.screenshot({
      path: join(root, "mobile-picker.png"),
      animations: "disabled",
    });
    await close();
    await attachment.waitFor();
    assert.equal(messages.length, 0);
    const ordinary = "Explain how /model appears in the documentation.";
    await composer.fill(ordinary);
    await page.locator("#send").click();
    await wait(
      () => messages.length === 1,
      "Ordinary message reaches the message endpoint",
    );
    assert.equal(messages[0].text, ordinary);
    assert.equal(messages[0].room, main.id);
    assert.equal(
      messages[0].assets.length,
      1,
      "The retained attachment belongs to the ordinary message",
    );
    assert.deepEqual(
      focusChecks,
      focusChecks.map(() => true),
      "Escape returns focus to the composer or its Send trigger",
    );
    expect(errors).toEqual([]);
    console.log(
      "PASS mobile Send opens picker; ordinary messages and retained attachment still send",
    );
    console.log(
      JSON.stringify({
        browser: browser.browserType().name(),
        evidence: root,
        settings: settings.length,
        settingRequests: settings,
        staleEntityVersions,
        messages: messages.length,
      }),
    );
  } catch (error) {
    console.error(error);
    await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
    throw error;
  } finally {
    await page?.unrouteAll({ behavior: "ignoreErrors" });
    await context?.close();
  }
});
