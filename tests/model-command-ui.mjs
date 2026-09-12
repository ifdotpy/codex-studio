#!/usr/bin/env node
// Production renderer, isolated runtime, real settings and message endpoints.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
const repo = join(import.meta.dirname, "..");
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-model-command-ui-"));
const models = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"].map((model) => ({
  model,
  defaultReasoningEffort: "medium",
  supportedReasoningEfforts: (model === "gpt-5.6-luna"
    ? ["low", "medium", "high", "max"]
    : ["low", "medium", "high", "ultra"]
  ).map((reasoningEffort) => ({ reasoningEffort })),
  serviceTiers: [{ id: "priority" }],
}));
const proc = spawn(
  process.env.PYTHON || "/opt/homebrew/bin/python3",
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
let browser,
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
  const main = initial.threads.find((agent) => agent.name === "Other project");
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const worker = initial.threads.find((agent) => agent.name === "Worker 00");
  const agent = async (id) =>
    (await snapshot()).runtime.agents.find((value) => value.id === id);
  const wait = async (test, label) => {
    for (let i = 0; i < 100; i++) {
      if (await test()) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error(label + "\n" + log);
  };
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
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
  await page.route("**/api/sync/pull?*", async (route) => {
    const scope = new URL(route.request().url()).searchParams.get("scope");
    if (!lagMainSettings || !["state", "state:chat"].includes(scope))
      return route.continue();
    const response = await route.fetch();
    const data = await response.json();
    for (const document of data.documents || []) {
      const payload = JSON.parse(document.payload);
      for (const value of [
        ...(payload.threads || []),
        ...(payload.runtime?.agents || []),
      ]) {
        if (value.id === main.id) {
          value.model = main.model;
          value.effort = main.effort;
        }
      }
      document.payload = JSON.stringify(payload);
      laggedProjections++;
    }
    await route.fulfill({ response, json: data });
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
  let openedBySend = false;
  const open = async (text = "/model", send = false, worker = false) => {
    openedBySend = send;
    await composer.fill(text);
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
  lagMainSettings = true;
  await dialog()
    .getByLabel("Main agent model", { exact: true })
    .selectOption("gpt-5.6-sol");
  await wait(
    async () => (await agent(main.id)).model === "gpt-5.6-sol",
    "Main model persists",
  );
  await dialog()
    .getByLabel("Main agent reasoning", { exact: true })
    .selectOption("high");
  await wait(
    async () => (await agent(main.id)).effort === "high",
    "Main reasoning persists",
  );
  await wait(
    () => laggedProjections > 0,
    "The replica returned stale main settings",
  );
  assert.equal(messages.length, 0);
  assert(
    settings
      .filter((body) => body.id === main.id)
      .every(
        (body) => body.expected_account_key === (main.accountKey || "default"),
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
    () => dialog().getByLabel("Main agent model", { exact: true }).isEnabled(),
    "Reopened catalog ready",
  );
  const reopened = await dialog()
    .getByLabel("Main agent model", { exact: true })
    .inputValue();
  if (reopened !== "gpt-5.6-sol")
    console.error("Reopened model:", {
      shown: reopened,
      stored: (await agent(main.id)).model,
      requests: settings
        .filter((body) => body.id === main.id)
        .map(({ model, effort, next_turn }) => ({ model, effort, next_turn })),
    });
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
  await dialog(true)
    .getByLabel("Subagent model", { exact: true })
    .selectOption("gpt-5.6-sol");
  await wait(
    async () =>
      (await agent(worker.id)).pendingSettings?.model === "gpt-5.6-sol",
    "Active worker queues selected model",
  );
  await dialog(true)
    .getByLabel("Subagent reasoning", { exact: true })
    .selectOption("medium");
  await wait(
    async () => (await agent(worker.id)).pendingSettings?.effort === "medium",
    "Active worker queues reasoning",
  );
  const running = await agent(worker.id);
  assert.equal(running.model, worker.model);
  assert.equal(running.effort, worker.effort);
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
    () => dialog().getByLabel("Main agent model", { exact: true }).isEnabled(),
    "Mobile catalog ready",
  );
  assert.equal(
    await dialog().getByLabel("Main agent model", { exact: true }).inputValue(),
    "gpt-5.6-sol",
    "Mobile picker retains the saved main model",
  );
  await dialog()
    .getByLabel("Main agent reasoning", { exact: true })
    .selectOption("medium");
  await wait(
    async () => (await agent(main.id)).effort === "medium",
    "Mobile picker persists reasoning",
  );
  assert.equal(
    await dialog().evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
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
  assert.deepEqual(errors, []);
  console.log(
    "PASS mobile Send opens picker; ordinary messages and retained attachment still send",
  );
  console.log(
    JSON.stringify({
      browser: browserType.name(),
      evidence: root,
      settings: settings.length,
      messages: messages.length,
    }),
  );
} catch (error) {
  console.error(error);
  await page?.screenshot({ path: join(root, "failure.png") }).catch(() => {});
  throw error;
} finally {
  await page?.unrouteAll({ behavior: "ignoreErrors" });
  await browser?.close();
  if (proc.exitCode === null) {
    const exit = once(proc, "exit");
    proc.kill("SIGTERM");
    await exit;
  }
}
