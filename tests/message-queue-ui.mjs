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
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  const composer = page.locator("#message"),
    panel = page.getByTestId("message-queue"),
    list = page.getByRole("list", { name: "Queued messages", exact: true });
  const button = (name) => panel.getByRole("button", { name, exact: true });
  const editor = () =>
    panel.getByRole("textbox", { name: "Edit queued message", exact: true });
  const attach = async (name) => {
    await page.getByLabel("Choose attachments").setInputFiles({
      name,
      mimeType: "text/plain",
      buffer: Buffer.from("Queue attachment fixture"),
    });
    await page
      .getByRole("button", { name: `Remove ${name}`, exact: true })
      .waitFor();
  };
  await composer.fill("");
  await composer.press("Tab");
  assert.equal(
    await composer.evaluate((node) => node === document.activeElement),
    false,
  );
  assert.equal(sent.length, 0);
  await attach("queued-evidence.txt");
  await composer.fill("First queued instruction");
  await composer.press("Tab");
  await wait(
    async () => (await queue()).items.length === 1,
    "Tab queues one message",
  );
  assert.equal(sent.length, 1);
  assert.equal(sent[0].delivery, "queue");
  const originalAsset = (await queue()).items[0].assets[0].id;
  await composer.fill("Keyboard navigation draft");
  for (const key of [
    { repeat: true },
    { isComposing: true },
    { ctrlKey: true },
    { altKey: true },
    { metaKey: true },
  ]) {
    assert.equal(
      await composer.evaluate(
        (node, extra) =>
          node.dispatchEvent(
            new KeyboardEvent("keydown", {
              key: "Tab",
              bubbles: true,
              cancelable: true,
              ...extra,
            }),
          ),
        key,
      ),
      true,
      "Modified Tab is not intercepted",
    );
  }
  await composer.press("Shift+Tab");
  assert.equal(
    await composer.evaluate((node) => node === document.activeElement),
    false,
  );
  assert.equal(sent.length, 1);
  assert.equal(await composer.inputValue(), "Keyboard navigation draft");
  await composer.fill("/model");
  await composer.press("Tab");
  assert.equal(sent.length, 1);
  assert.equal(await composer.inputValue(), "/model");
  assert.equal(
    await composer.evaluate((node) => node === document.activeElement),
    false,
  );
  await composer.press("Enter");
  const model = page.getByRole("dialog", {
    name: "Main agent settings",
    exact: true,
  });
  await model.waitFor();
  await page.keyboard.press("Escape");
  await model.waitFor({ state: "hidden" });
  assert.equal(sent.length, 1);
  await composer.fill("Second queued instruction");
  await composer.press("Enter");
  await wait(
    async () => (await queue()).items.length === 2,
    "Enter keeps after-tool delivery",
  );
  assert.equal(sent[1].delivery, "after_tool");
  await composer.fill("Third queued instruction");
  await composer.press("Tab");
  await wait(
    async () => (await queue()).items.length === 3,
    "Third message enters queue",
  );
  await panel.waitFor();
  await list.getByText("Third queued instruction", { exact: true }).waitFor();
  for (const text of [
    "First queued instruction",
    "Second queued instruction",
    "Third queued instruction",
  ]) {
    assert.equal(
      await page
        .locator("#messages .message.user")
        .filter({ hasText: text })
        .count(),
      0,
      "Queued text is not duplicated in the transcript",
    );
  }
  console.log(
    "PASS Tab queues once; modifiers/navigation and /model retained; Enter uses after_tool; no duplicate transcript bubbles",
  );
  const draft = "Keep this separate composer draft";
  await composer.fill(draft);
  await attach("composer-evidence.txt");
  await button("Edit queued message 1").click();
  await editor().fill("Revised first instruction");
  await button("Save queued message").click();
  await wait(
    async () => (await queue()).items[0].text === "Revised first instruction",
    "Edit persisted",
  );
  assert.equal(await composer.inputValue(), draft);
  await page
    .getByRole("button", { name: "Remove composer-evidence.txt", exact: true })
    .waitFor();
  assert.equal(
    (await queue()).items[0].assets[0].id,
    originalAsset,
    "Queue edits preserve queued assets",
  );
  await button("Move queued message 3 up").click();
  await wait(
    async () =>
      (await queue()).items.map((item) => item.text).join("|") ===
      "Revised first instruction|Third queued instruction|Second queued instruction",
    "Button reorder persists",
  );
  const beforeDrag = (await queue()).items.map((item) => item.id);
  await list
    .locator("[draggable=true]")
    .first()
    .dragTo(list.locator("li[data-message-id]").last());
  await wait(
    async () =>
      (await queue()).items.map((item) => item.id).join("|") !==
      beforeDrag.join("|"),
    "Drag reorder changes durable order",
  );
  const reordered = (await queue()).items.map((item) => item.id);
  assert.deepEqual([...reordered].sort(), [...beforeDrag].sort());
  await wait(
    () => panel.getAttribute("aria-busy").then((value) => value === "false"),
    "Reorder receipt settled before reload",
  );
  diagnostics.push({ kind: "reload", at: Date.now() });
  await page.reload();
  await panel.waitFor();
  await wait(
    async () =>
      (await queue()).items.map((item) => item.id).join("|") ===
      reordered.join("|"),
    "Order survives reload",
  );
  await wait(
    async () =>
      (
        await list
          .locator("li[data-message-id]")
          .evaluateAll((nodes) => nodes.map((node) => node.dataset.messageId))
      ).join("|") === reordered.join("|"),
    "Reloaded panel uses the saved order",
  );
  await wait(
    () => composer.inputValue().then((value) => value === draft),
    "Composer draft survives queue reload",
  );
  await page
    .getByRole("button", { name: "Remove composer-evidence.txt", exact: true })
    .waitFor();
  for (const text of (await queue()).items.map((item) => item.text))
    assert.equal(
      await page
        .locator("#messages .message.user")
        .filter({ hasText: text })
        .count(),
      0,
    );
  console.log(
    "PASS edit preserves draft and both attachment sets; buttons/drag reorder persist after reload",
  );
  await button("Edit queued message 1").click();
  await editor().fill("Recovered queue draft");
  diagnostics.push({ kind: "reload", at: Date.now() });
  await page.reload();
  await editor().waitFor();
  assert.equal(await editor().inputValue(), "Recovered queue draft");
  await editor().fill("Saved recovered queue draft");
  await button("Save queued message").click();
  await wait(
    async () => (await queue()).items[0].text === "Saved recovered queue draft",
    "Recovered edit saves",
  );
  await editor().waitFor({ state: "hidden" });
  await wait(
    () => panel.getAttribute("aria-busy").then((value) => value === "false"),
    "Recovered edit receipt settled",
  );
  diagnostics.push({ kind: "reload", at: Date.now() });
  await page.reload();
  await panel.waitFor();
  assert.equal(
    await editor().count(),
    0,
    "A saved recovered edit does not return after reload",
  );
  await page.screenshot({
    path: join(root, "queue-desktop-three.png"),
    animations: "disabled",
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await button("Edit queued message 2").click();
  await editor().fill(
    "A mobile editor stays inside the queue with three messages.",
  );
  assert.equal(
    await panel.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
    true,
  );
  await page.screenshot({
    path: join(root, "queue-mobile-three-editor.png"),
    animations: "disabled",
  });
  await page.setViewportSize({ width: 390, height: 480 });
  await button("Save queued message").scrollIntoViewIfNeeded();
  assert.equal(await button("Save queued message").isVisible(), true);
  assert.equal(await composer.isVisible(), true);
  for (const control of [
    panel.locator(".message-queue-heading"),
    button("Save queued message"),
    composer,
  ]) {
    const bounds = await control.boundingBox();
    assert.ok(
      bounds && bounds.y >= 0 && bounds.y + bounds.height <= 480,
      "Short viewport retains the queue heading, Save button, and composer",
    );
  }
  assert.equal(
    await panel.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
    true,
  );
  await page.screenshot({
    path: join(root, "queue-mobile-short-editor.png"),
    animations: "disabled",
  });
  await button("Cancel edit").click();
  await page.setViewportSize({ width: 1440, height: 960 });
  console.log(
    "PASS recovered edit saves without resurrection; three-row mobile editor fits",
  );
  let hold = null;
  let loseReply = false,
    lostBody;
  await page.route("**/api/queue", async (route) => {
    const body = route.request().postDataJSON();
    if (loseReply && body.action === "edit") {
      loseReply = false;
      lostBody = body;
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      await route.abort("failed");
      return;
    }
    if (hold && body.action === "edit") {
      const current = hold;
      hold = null;
      current.body = body;
      current.ready();
      await current.wait;
    }
    await route.continue();
  });
  const intercept = () => {
    let ready, release;
    const result = {
      wait: new Promise((resolve) => {
        release = resolve;
      }),
      ready: () => ready(),
      body: null,
    };
    const received = new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(Error("The edit did not reach the HTTP interception")),
        10000,
      );
      ready = () => {
        clearTimeout(timer);
        resolve();
      };
    });
    hold = result;
    return { received, release, result };
  };
  await button("Edit queued message 1").click();
  await editor().fill("Local edit remains visible");
  const conflict = intercept();
  await button("Save queued message").click({ noWaitAfter: true });
  await conflict.received;
  let current = await queue(),
    target = current.items[0];
  await post({
    action: "edit",
    message_id: target.id,
    text: "Changed by another client",
    expectedText: target.text,
    expected_revision: current.revision,
  });
  conflict.release();
  await panel.getByRole("alert").waitFor();
  await editor().waitFor();
  assert.equal(await editor().inputValue(), "Local edit remains visible");
  assert.equal(
    (await queue()).items.find((item) => item.id === target.id).text,
    "Changed by another client",
  );
  await button("Cancel edit").click();
  assert.equal(await composer.inputValue(), draft);
  await button("Edit queued message 1").click();
  await editor().fill("Must not resurrect a dequeued message");
  const leaving = intercept();
  await button("Save queued message").click({ noWaitAfter: true });
  await leaving.received;
  current = await queue();
  target = current.items[0];
  await post({
    action: "cancel",
    message_id: target.id,
    expectedText: target.text,
    expected_revision: current.revision,
  });
  leaving.release();
  await wait(
    async () => (await queue()).items.length === 2,
    "Concurrent dequeue persists",
  );
  await wait(
    async () => (await list.locator("li[data-message-id]").count()) === 2,
    "Dequeued row leaves the panel",
  );
  assert.equal(
    await list
      .getByText("Must not resurrect a dequeued message", { exact: true })
      .count(),
    0,
  );
  assert.equal(sent.length, 3);
  await button("Discard saved queue edit").click();
  await button("Delete queued message 1").click();
  await wait(async () => (await queue()).items.length === 1, "Delete persists");
  console.log(
    "PASS concurrent edit keeps local buffer; dequeue race cannot restore a row or send another message",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await panel.waitFor();
  await button("Edit queued message 1").click();
  await editor().fill("Mobile queued instruction");
  await button("Save queued message").click();
  await wait(
    async () => (await queue()).items[0].text === "Mobile queued instruction",
    "Mobile edit persists",
  );
  assert.equal(await composer.inputValue(), draft);
  await page
    .getByRole("button", { name: "Remove composer-evidence.txt", exact: true })
    .waitFor();
  assert.equal(
    await panel.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
    true,
  );
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
    true,
  );
  await page.screenshot({
    path: join(root, "queue-mobile.png"),
    animations: "disabled",
  });
  assert.equal(sent.length, 3);
  const peer = await page.context().newPage();
  await peer.goto(origin);
  await peer.getByTestId("message-queue").waitFor();
  await button("Edit queued message 1").click();
  await editor().fill("Confirmed after lost reply");
  loseReply = true;
  await button("Save queued message").click();
  const retry = () =>
    page.getByRole("button", { name: "Retry queue change", exact: true });
  await retry().waitFor();
  await peer
    .getByRole("button", { name: "Retry queue change", exact: true })
    .waitFor();
  const applied = await queue();
  assert.equal(applied.items[0].text, "Confirmed after lost reply");
  await peer.close();
  diagnostics.push({ kind: "reload", at: Date.now() });
  await page.reload();
  await retry().waitFor();
  await retry().click();
  await wait(
    async () =>
      (await retry().count()) === 0 &&
      (await panel.getAttribute("aria-busy")) === "false",
    "Lost response retry settles",
  );
  assert.equal(
    (await queue()).revision,
    applied.revision,
    "Exact retry does not apply the edit again",
  );
  const retries = mutations.filter(
    (body) => body.request_id === lostBody.request_id,
  );
  assert.equal(retries.length, 2);
  assert.deepEqual(
    retries[0],
    retries[1],
    "Reload retries the exact stored request",
  );
  assert.equal(sent.length, 3);
  console.log(
    "PASS lost response is visible across tabs; reload retries exact receipt without another edit",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS 390px queue controls and layout; queue mutations never send extra messages",
  );
  console.log(
    JSON.stringify({
      browser: browserType.name(),
      evidence: root,
      messages: sent.length,
      mutations: mutations.length,
    }),
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
