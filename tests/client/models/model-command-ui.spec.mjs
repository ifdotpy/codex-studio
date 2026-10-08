import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  readTestState,
  test,
  expect,
  spawnFixture as spawn,
} from "../playwright.mjs";
import { randomUUID } from "node:crypto";
import { chooseSetupValue } from "../../setup-controls.mjs";
// Production renderer, isolated runtime, real settings and message endpoints.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { modelValue, selectModel } from "../../model-picker.mjs";

test("model-command-ui", async ({ browser }) => {
  test.setTimeout(45_000);
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
    secondContext,
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
    const snapshot = () => readTestState(origin);
    const initial = await snapshot();
    const main = initial.threads.find(
      (agent) => agent.name === "Other project",
    );
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const worker = initial.threads.find((agent) => agent.name === "Worker 00");
    const agent = async (id) =>
      (await snapshot()).runtime.agents.find((value) => value.id === id);
    // Read persisted settings independently from the renderer's sync projection.
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
    secondContext = await browser.newContext({
      viewport: { width: 1440, height: 960 },
    });
    const secondPage = await secondContext.newPage();
    let staleReplica = false;
    let committedMainEntity;
    let committedMainDocument;
    let staleMainEntity;
    await secondContext.route("**/api/sync/pull?*", async (route) => {
      const url = new URL(route.request().url());
      const response = await route.fetch();
      if (url.searchParams.get("scope") !== "state:entities:v1")
        return route.fulfill({ response });
      const body = await response.json();
      const mainDocument = (body.documents || []).find((document) => {
        const payload = JSON.parse(document.payload);
        return payload.collection === "agent" && payload.value?.id === main.id;
      });
      if (!staleReplica && !mainDocument)
        return route.fulfill({ response, json: body });
      if (!staleReplica) {
        const payload = JSON.parse(mainDocument.payload);
        committedMainDocument = structuredClone(mainDocument);
        committedMainEntity = {
          seq: mainDocument.seq,
          model: payload.value.model,
          effort: payload.value.effort,
        };
        return route.fulfill({ response, json: body });
      }
      const staleDocument =
        mainDocument || structuredClone(committedMainDocument);
      const payload = JSON.parse(staleDocument.payload);
      staleMainEntity = {
        seq: committedMainEntity.seq - 1,
        model: main.model,
        effort: main.effort,
      };
      staleDocument.seq = staleMainEntity.seq;
      payload.value.model = staleMainEntity.model;
      payload.value.effort = staleMainEntity.effort;
      staleDocument.payload = JSON.stringify(payload);
      if (!mainDocument) body.documents.push(staleDocument);
      return route.fulfill({ response, json: body });
    });
    const entityPull = secondPage.waitForResponse((response) => {
      const url = new URL(response.url());
      return (
        url.pathname === "/api/sync/pull" &&
        url.searchParams.get("scope") === "state:entities:v1"
      );
    });
    await secondPage.addInitScript(
      ({ id, stateDir }) => {
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        );
        localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
      },
      { id: main.id, stateDir: initial.stateDir },
    );
    const streamConnected = secondPage.waitForResponse((response) => {
      return new URL(response.url()).pathname === "/api/sync/stream";
    });
    await secondPage.goto(origin);
    const entityBody = await (await entityPull).json();
    const mainEntity = entityBody.documents
      .map((document) => ({
        document,
        payload: JSON.parse(document.payload),
      }))
      .find(
        ({ payload }) =>
          payload.collection === "agent" && payload.value?.id === main.id,
      );
    assert(mainEntity, "The second renderer pulled the main agent entity");
    const secondEntitySettings = {
      collection: mainEntity.payload.collection,
      id: mainEntity.payload.value.id,
      model: mainEntity.payload.value.model,
      effort: mainEntity.payload.value.effort,
      sequence: mainEntity.document.seq,
    };
    console.log("Second renderer sync entity:", secondEntitySettings);
    assert(
      Number.isSafeInteger(secondEntitySettings.sequence) &&
        secondEntitySettings.sequence > 0,
    );
    await secondPage.locator(`[data-chat="${main.id}"]`).click();
    const secondComposer = secondPage.locator("#message");
    await secondComposer.fill("/model");
    await secondComposer.press("Enter");
    const secondDialog = secondPage.getByRole("dialog", {
      name: "Main agent settings",
      exact: true,
    });
    await secondDialog.waitFor();
    const secondRendererSettings = {
      model: await modelValue(
        secondDialog.getByLabel("Main agent model", { exact: true }),
      ),
      effort: await secondDialog
        .getByLabel("Main agent reasoning", { exact: true })
        .getAttribute("data-value"),
    };
    console.log("Second renderer settings:", secondRendererSettings);
    assert.deepEqual(
      {
        collection: secondEntitySettings.collection,
        id: secondEntitySettings.id,
        model: secondEntitySettings.model,
        effort: secondEntitySettings.effort,
      },
      {
        collection: "agent",
        id: main.id,
        model: "gpt-5.6-sol",
        effort: "high",
      },
    );
    assert.deepEqual(secondRendererSettings, {
      model: "gpt-5.6-sol",
      effort: "high",
    });
    assert.deepEqual(committedMainEntity, {
      seq: secondEntitySettings.sequence,
      model: "gpt-5.6-sol",
      effort: "high",
    });
    await streamConnected;
    staleReplica = true;
    const stalePull = secondPage.waitForResponse((response) => {
      const url = new URL(response.url());
      return (
        url.pathname === "/api/sync/pull" &&
        url.searchParams.get("scope") === "state:entities:v1"
      );
    });
    const notifyResponse = await fetch(new URL("/api/sync/notify", origin), {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": initial.token,
        [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
      },
      body: JSON.stringify({
        requestId: randomUUID(),
        resources: [{ kind: "state" }],
      }),
    });
    assert.equal(
      notifyResponse.status,
      200,
      "The fixture accepted the state invalidation",
    );
    await stalePull;
    assert(
      staleMainEntity,
      "The second renderer received the lagging main-agent row",
    );
    assert(staleMainEntity.seq < committedMainEntity.seq);
    assert.deepEqual(
      { model: staleMainEntity.model, effort: staleMainEntity.effort },
      { model: main.model, effort: main.effort },
    );
    const afterStaleReplica = {
      model: await modelValue(
        secondDialog.getByLabel("Main agent model", { exact: true }),
      ),
      effort: await secondDialog
        .getByLabel("Main agent reasoning", { exact: true })
        .getAttribute("data-value"),
    };
    console.log("Lower-sequence stale entity and second renderer:", {
      committed: committedMainEntity,
      stale: staleMainEntity,
      shown: afterStaleReplica,
    });
    assert.deepEqual(
      afterStaleReplica,
      { model: "gpt-5.6-sol", effort: "high" },
      "A lower-sequence stale entity cannot overwrite saved main settings",
    );
    assert.equal(messages.length, 0);
    assert(
      settings
        .filter((body) => body.id === main.id)
        .every(
          (body) =>
            body.expected_account_key === (main.accountKey || "default"),
        ),
    );
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
      "Reopened picker keeps saved reasoning",
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
    await chooseSetupValue(
      dialog(true).getByLabel("Subagent reasoning", { exact: true }),
      "medium",
    );
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
        messages: messages.length,
      }),
    );
  } catch (error) {
    console.error(error);
    await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
    throw error;
  } finally {
    await page?.unrouteAll({ behavior: "ignoreErrors" });
    await secondContext?.close();
    await context?.close();
  }
});
