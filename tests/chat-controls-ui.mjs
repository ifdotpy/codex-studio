#!/usr/bin/env node
// Exercise production components against isolated HTTP and SQLite, with one transport failure.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(skill, "web/package.json"));
const { chromium } = require("playwright-core");
const root = await mkdtemp(join(tmpdir(), "codex-chat-controls-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let log = "",
  browser;
fixture.stderr.on("data", (data) => (log += data));
const poll = async (fn, label) => {
  for (let i = 0; i < 120; i++) {
    if (await fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw Error(`${label}\n${log}`);
};
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = () =>
    fetch(`${origin}/api/state`).then((response) => response.json());
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
    args: ["--disable-extensions", "--no-first-run"],
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  // Durable attachments use the real workspace identity and sync store.
  await page.goto(origin);
  await page.locator("[data-chat]").first().waitFor();
  const initial = await state();
  const lead = initial.threads.find((agent) => agent.name === "Other project");
  const row = () => page.locator(`[data-chat="${lead.id}"]`);
  await row().click();
  const actions = () =>
    page.getByRole("button", {
      name: "Actions for Other project",
      exact: true,
    });
  await row().hover();
  await actions().click();
  await page.getByRole("menuitem", { name: "Pin", exact: true }).click();
  await poll(
    async () =>
      (await state()).threads.find((agent) => agent.id === lead.id).pinned,
    "pin is persisted",
  );
  const folder = join(root, "Release checks");
  await mkdir(folder);
  await row().hover();
  await actions().click();
  await page
    .getByRole("menuitem", { name: "Change project folder", exact: true })
    .click();
  const folderDialog = page.getByRole("dialog", {
    name: "Choose project folder",
    exact: true,
  });
  await folderDialog.getByLabel("Folder path").fill(folder);
  await folderDialog.getByRole("button", { name: "Go", exact: true }).click();
  await folderDialog
    .getByRole("button", { name: "Use this folder", exact: true })
    .click();
  await folderDialog.waitFor({ state: "hidden" });
  await poll(
    async () =>
      (await state()).threads
        .find((a) => a.id === lead.id)
        .cwd.endsWith("/Release checks"),
    "project directory is persisted",
  );
  await row().hover();
  await actions().click();
  await page.getByRole("menuitem", { name: "Archive", exact: true }).click();
  await poll(
    async () => (await row().count()) === 0,
    "archived chat leaves active list",
  );
  await page
    .getByRole("button", { name: "Project list options", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Show archived chats", exact: true })
    .click();
  await row().waitFor();
  await row().hover();
  await actions().click();
  await page
    .getByRole("menuitem", { name: "Restore chat", exact: true })
    .click();
  await poll(
    async () => (await row().count()) === 0,
    "restored chat leaves archive list",
  );
  await page
    .getByRole("button", { name: "Project list options", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Show active chats", exact: true })
    .click();
  await row().click();

  await page.locator('input[type="file"]').setInputFiles({
    name: "evidence.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("Evidence for this turn.\n"),
  });
  await page
    .getByRole("button", { name: "Remove evidence.txt", exact: true })
    .waitFor();
  await page.reload();
  await row().click();
  await page
    .getByRole("button", { name: "Remove evidence.txt", exact: true })
    .waitFor();
  await page.locator("#message").evaluate((element) => {
    const bytes = Uint8Array.from(
      atob(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6DcgAAAAASUVORK5CYII=",
      ),
      (char) => char.charCodeAt(0),
    );
    const transfer = new DataTransfer();
    transfer.items.add(new File([bytes], "pasted.png", { type: "image/png" }));
    element.dispatchEvent(
      new ClipboardEvent("paste", {
        clipboardData: transfer,
        bubbles: true,
        cancelable: true,
      }),
    );
  });
  await page
    .getByRole("button", { name: "Remove pasted.png", exact: true })
    .waitFor();
  await page.locator("#composer").evaluate((element) => {
    const transfer = new DataTransfer();
    transfer.items.add(
      new File(["Dropped evidence"], "dropped.txt", { type: "text/plain" }),
    );
    element.dispatchEvent(
      new DragEvent("drop", {
        dataTransfer: transfer,
        bubbles: true,
        cancelable: true,
      }),
    );
  });
  await page
    .getByRole("button", { name: "Remove dropped.txt", exact: true })
    .waitFor();
  assert.equal(
    await page.locator("#send").isEnabled(),
    true,
    "attachment-only message is enabled",
  );
  const attachmentPosts = [];
  page.on("request", (request) => {
    if (
      request.method() === "POST" &&
      new URL(request.url()).pathname === "/api/messages"
    )
      attachmentPosts.push(request.postDataJSON());
  });
  let failAttachments = true;
  const failAttachmentSend = (route) =>
    failAttachments
      ? route.fulfill({
          status: 503,
          json: { error: "Test transport failure" },
        })
      : route.continue();
  await page.route("**/api/messages", failAttachmentSend);
  await page.locator("#send").click();
  await page.getByText("Test transport failure", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Stop retries", exact: true }).click();
  const firstAttempt = attachmentPosts[0];
  assert.ok(firstAttempt.id, "Failed send has a durable message identity");
  const failedMessage = page.locator("#messages .message.user").filter({
    has: page.getByRole("button", { name: "evidence.txt", exact: true }),
  });
  for (const name of ["evidence.txt", "dropped.txt"])
    await failedMessage.getByRole("button", { name, exact: true }).waitFor();
  await failedMessage.locator('img[alt="pasted.png"]').waitFor();
  assert.equal(
    await page
      .getByRole("button", { name: "Remove evidence.txt", exact: true })
      .count(),
    0,
    "The durable outbox owns attachments after the composer clears",
  );
  failAttachments = false;
  await failedMessage
    .getByRole("button", { name: "Resume retries", exact: true })
    .click();
  await poll(
    () => attachmentPosts.length >= 2,
    "Outbox retries the attachment message",
  );
  await poll(
    async () =>
      (await state()).threads.find((agent) => agent.id === lead.id).status ===
      "running",
    "attachment starts actual fixture turn",
  );
  for (const attempt of attachmentPosts)
    assert.deepEqual(
      attempt,
      firstAttempt,
      "Retry preserves the message identity and all attachment IDs",
    );
  await page.unroute("**/api/messages", failAttachmentSend);
  await page
    .locator("#messages")
    .getByRole("button", { name: "evidence.txt", exact: true })
    .waitFor();
  await page
    .locator("#messages")
    .getByRole("button", { name: "evidence.txt", exact: true })
    .click();
  const preview = page.getByRole("dialog", {
    name: "evidence.txt",
    exact: true,
  });
  await preview
    .getByText("Evidence for this turn.", { exact: false })
    .waitFor();
  await page.keyboard.press("Escape");
  await preview.waitFor({ state: "hidden" });
  await page.locator('#messages img[alt="pasted.png"]').waitFor();
  await poll(
    () =>
      page
        .locator('#messages img[alt="pasted.png"]')
        .evaluate((image) => image.naturalWidth > 0),
    "inline image decodes",
  );
  const queue = () =>
    fetch(`${origin}/api/queue?agent=${lead.id}`).then((response) =>
      response.json(),
    );
  for (const text of [
    "First queued instruction",
    "Second queued instruction",
  ]) {
    await page.locator("#message").fill(text);
    await page
      .getByRole("button", { name: "Queue after turn", exact: true })
      .click();
    await poll(
      async () => (await queue()).items.some((item) => item.text === text),
      "message enters durable queue",
    );
  }
  const queuePanel = page.getByTestId("message-queue");
  await queuePanel.waitFor();
  const queueList = queuePanel.getByRole("list", {
    name: "Queued messages",
    exact: true,
  });
  await queueList
    .getByText("First queued instruction", { exact: true })
    .waitFor();
  for (const text of ["First queued instruction", "Second queued instruction"])
    assert.equal(
      await page
        .locator("#messages .message.user")
        .filter({ hasText: text })
        .count(),
      0,
      "Queued messages appear only in the queue panel",
    );
  await queuePanel
    .getByRole("button", { name: "Edit queued message 1", exact: true })
    .click();
  await queuePanel
    .getByRole("textbox", { name: "Edit queued message", exact: true })
    .fill("Revised first instruction");
  await queuePanel
    .getByRole("button", { name: "Save queued message", exact: true })
    .click();
  await poll(
    async () => (await queue()).items[0].text === "Revised first instruction",
    "queue edit is persisted",
  );
  await queuePanel
    .getByRole("button", { name: "Move queued message 2 up", exact: true })
    .click();
  await poll(
    async () => (await queue()).items[0].text === "Second queued instruction",
    "queue order is persisted",
  );
  await poll(
    async () =>
      (await queueList.locator("li").first().textContent()).includes(
        "Second queued instruction",
      ),
    "queue UI applies server order",
  );
  await queuePanel
    .getByRole("button", { name: "Delete queued message 1", exact: true })
    .click();
  await poll(
    async () => (await queue()).items.length === 1,
    "queue cancellation is persisted",
  );
  assert.equal(
    await page
      .getByRole("group", { name: "Message delivery", exact: true })
      .count(),
    0,
  );
  await page.locator("#message").fill("");
  await page.locator("#message").press("Tab");
  assert.equal(
    await page
      .locator("#message")
      .evaluate((e) => e === document.activeElement),
    false,
    "empty Tab retains keyboard navigation",
  );
  await page.locator("#message").fill("Keyboard check");
  const priorQueueLength = (await queue()).items.length;
  await page.locator("#message").press("Tab");
  await poll(
    async () => (await queue()).items.length === priorQueueLength + 1,
    "Tab queues a nonempty draft once",
  );
  await poll(
    async () => (await page.locator("#message").inputValue()) === "",
    "Tab clears the accepted draft",
  );
  await page.locator("#message").fill("Keyboard navigation draft");
  await page.locator("#message").press("Shift+Tab");
  assert.equal(
    await page
      .locator("#message")
      .evaluate((e) => e === document.activeElement),
    false,
    "Shift+Tab retains keyboard navigation",
  );
  await page.locator("#message").fill("");
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
  );
  await page.screenshot({ path: join(root, "message-shortcuts-mobile.png") });
  await page.setViewportSize({ width: 1440, height: 960 });
  await page
    .locator("#message")
    .fill("Steering failure keeps this instruction");
  await page.locator("#send").click();
  const failedSteer = page
    .locator("#messages .message.user")
    .filter({ hasText: "Steering failure keeps this instruction" });
  await failedSteer.waitFor();
  await poll(
    () => log.includes("AssertionError: turn/steer"),
    "The fixture rejects the steer attempt",
  );
  await failedSteer
    .getByRole("button", { name: "Stop retries", exact: true })
    .click();
  await failedSteer
    .getByRole("button", { name: "Resume retries", exact: true })
    .waitFor();
  assert.equal(
    await page.locator("#message").inputValue(),
    "",
    "The durable outbox retains the failed steer outside the composer",
  );
  assert.equal(
    (await queue()).items.length,
    priorQueueLength + 1,
    "steer failure does not silently queue",
  );

  await page.locator("#message").fill("Keyboard instruction");
  await page.route(
    "**/api/messages",
    (r) =>
      r.fulfill({ status: 503, json: { error: "Keyboard transport failure" } }),
    { times: 1 },
  );
  const entered = page.waitForRequest(
    (r) => r.url().endsWith("/api/messages") && r.method() === "POST",
  );
  await page.locator("#message").press("Enter");
  assert.equal(
    (await entered).postDataJSON().delivery,
    "after_tool",
    "Enter uses after-tool-call delivery",
  );
  await page
    .locator("#messages .message.user")
    .filter({ hasText: "Keyboard instruction" })
    .waitFor();
  await page
    .locator("#message")
    .fill("Composer remains usable after a failed send");
  assert.equal(await page.locator("#send").isEnabled(), true);

  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.getByText("I assigned 40 workers", { exact: false }).waitFor();
  await page
    .locator("#messages")
    .getByRole("button", { name: "Quote message", exact: true })
    .last()
    .click();
  assert.match(
    await page.locator("#message").inputValue(),
    /^> /,
    "quote inserts quoted text",
  );
  await row().click();
  await page
    .locator("#messages")
    .getByRole("button", { name: "evidence.txt", exact: true })
    .waitFor();
  await page
    .getByTestId("message-queue")
    .getByText("Revised first instruction", { exact: true })
    .waitFor();
  await page.locator("#toast").waitFor({ state: "hidden" });
  await page.screenshot({ path: join(root, "chat-controls-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(root, "chat-controls-mobile.png") });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth + 1,
    ),
    "mobile has no horizontal overflow",
  );
  assert.deepEqual(errors, []);
  console.log(
    "Chat controls UI: PASS (attachment-only retry, draft reload, paste/drop, file preview, queue edit/order/cancel, steer failure, quote, pin/archive/project, mobile)",
  );
  console.log(`Browser evidence: ${root}`);
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
