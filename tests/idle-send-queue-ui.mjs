#!/usr/bin/env node
// Production renderer and isolated durable queue. No model service or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
const repo = join(import.meta.dirname, "..");
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-message-queue-ui-"));
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
const diagnostics = [];
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
  const initial = await (await fetch(origin + "/api/state")).json();
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const queue = () =>
    fetch(`${origin}/api/queue?agent=${lead.id}`).then((response) =>
      response.json(),
    );
  const post = async (body) => {
    const response = await fetch(origin + "/api/queue", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Origin: origin,
        "X-Canvas-Token": initial.token,
      },
      body: JSON.stringify({
        agent: lead.id,
        request_id: randomUUID(),
        ...body,
      }),
    });
    assert.equal(response.status, 200, await response.clone().text());
    return response.json();
  };
  for (const item of (await queue()).items) {
    const current = await queue();
    await post({
      action: "cancel",
      message_id: item.id,
      expectedText: item.text,
      expected_revision: current.revision,
    });
  }
  const wait = async (test, label) => {
    for (let i = 0; i < 120; i++) {
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
  // Fault injection must observe fetches after reload, including in WebKit.
  const context = await browser.newContext({
    viewport: { width: 1440, height: 960 },
    serviceWorkers: "block",
  });
  page = await context.newPage();
  page.setDefaultTimeout(10000);
  const errors = [],
    sent = [],
    mutations = [];
  page.on("pageerror", (error) => {
    errors.push(error.message);
    diagnostics.push({
      kind: "pageerror",
      message: error.message,
      stack: error.stack,
      at: Date.now(),
    });
  });
  page.on("requestfailed", (request) =>
    diagnostics.push({
      kind: "requestfailed",
      url: request.url(),
      failure: request.failure(),
      at: Date.now(),
    }),
  );
  page.on("request", (request) => {
    if (request.method() !== "POST") return;
    const path = new URL(request.url()).pathname;
    if (path === "/api/messages") sent.push(request.postDataJSON());
    if (path === "/api/queue") mutations.push(request.postDataJSON());
  });
  await page.addInitScript(
    ({ id, stateDir }) => {
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
      localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
    },
    { id: lead.id, stateDir: initial.stateDir },
  );
  let deliveredId = null;
  await page.route("**/api/transcript?*", async (route) => {
    const response = await route.fetch();
    const payload = await response.json();
    if (
      deliveredId &&
      new URL(route.request().url()).searchParams.get("id") === lead.id
    ) {
      payload.items = payload.items.map((item) =>
        item.clientMessageId === deliveredId ||
        item.id === `${lead.id}:${deliveredId}`
          ? {
              ...item,
              pending: false,
              materialized: true,
              deliveryStatus: "delivered",
            }
          : item,
      );
    }
    await route.fulfill({ response, json: payload });
  });
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  const composer = page.locator("#message"),
    panel = page.getByTestId("message-queue");
  const bubble = (text) =>
    page.locator("#messages .message.user").filter({ hasText: text });
  assert(!lead.inFlight && !lead.turnId, "Fixture chat has no active turn");
  await composer.fill("Normal idle send");
  await composer.press("Enter");
  await wait(async () => sent.length === 1, "Idle Send reaches scheduler");
  assert.equal(sent.length, 1);
  assert.equal(sent[0].delivery, "after_tool");
  // A message for after the current tool call is never queued. It stays in
  // the chat while it waits, also after a reload.
  await bubble("Normal idle send").waitFor();
  // Tab sends the draft for after the turn; it waits in the queue panel.
  await composer.fill("Tab queues this draft");
  await composer.press("Tab");
  await wait(async () => sent.length === 2, "Tab input reaches scheduler");
  assert.equal(sent[1].delivery, "after_turn");
  assert.equal(await composer.inputValue(), "");
  await panel.getByText("Tab queues this draft", { exact: true }).waitFor();
  assert.equal(await bubble("Tab queues this draft").count(), 0);
  await page.waitForTimeout(3500);
  assert.equal(
    await panel.getByText("Normal idle send", { exact: true }).count(),
    0,
    "an after-tool message is not in the queue",
  );
  await page.reload();
  await bubble("Normal idle send").waitFor();
  assert.equal(
    await panel.getByText("Normal idle send", { exact: true }).count(),
    0,
    "Reload keeps the after-tool message out of the queue",
  );
  assert.equal(sent.length, 2, "Reload does not send input again");
  assert.deepEqual(errors, []);
  console.log(
    "PASS idle send, Tab queues after the turn, after-tool messages stay out of the queue, reload identity",
  );
} catch (error) {
  console.error(error);
  console.error(JSON.stringify({ evidence: root, diagnostics }, null, 2));
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
