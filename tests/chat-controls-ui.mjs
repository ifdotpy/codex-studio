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
  // Exercise direct delivery against servers without the optional sync protocol.
  await page.route("**/api/sync/identity", (r) =>
    r.fulfill({ status: 404, json: { error: "Unsupported sync" } }),
  );
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
  await actions().click();
  await page.getByRole("menuitem", { name: "Pin", exact: true }).click();
  await poll(
    async () =>
      (await state()).threads.find((agent) => agent.id === lead.id).pinned,
    "pin is persisted",
  );
  const folder = join(root, "Release checks");
  await mkdir(folder);
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
  await page.route(
    "**/api/messages",
    (route) =>
      route.fulfill({
        status: 503,
        contentType: "application/json",
        body: JSON.stringify({ error: "Test transport failure" }),
      }),
    { times: 1 },
  );
  await page.locator("#send").click();
  await page.getByText("Test transport failure", { exact: true }).waitFor();
  assert.equal(
    await page
      .getByRole("button", { name: "Remove evidence.txt", exact: true })
      .count(),
    1,
    "failed send retains attachments",
  );
  await page.locator("#send").click();
  await poll(
    async () =>
      (await page
        .getByRole("button", { name: "Remove evidence.txt", exact: true })
        .count()) === 0,
    "successful retry clears attachments",
  );
  await poll(
    async () =>
      (await state()).threads.find((agent) => agent.id === lead.id).status ===
      "running",
    "attachment starts actual fixture turn",
  );
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
    await page.locator("#message").press("Tab");
    await poll(
      async () => (await queue()).items.some((item) => item.text === text),
      "message enters durable queue",
    );
  }
  await page
    .getByRole("button", { name: "2 queued messages", exact: true })
    .click();
  await page
    .locator(".queued-item")
    .first()
    .getByRole("button", { name: "Edit queued message", exact: true })
    .click();
  await page
    .getByRole("textbox", { name: "Edit queued message", exact: true })
    .fill("Revised first instruction");
  await page
    .locator(".queue-editor")
    .getByRole("button", { name: "Save", exact: true })
    .click();
  await poll(
    async () => (await queue()).items[0].text === "Revised first instruction",
    "queue edit is persisted",
  );
  await page
    .locator(".queued-item")
    .nth(1)
    .getByRole("button", { name: "Move message first", exact: true })
    .click();
  await poll(
    async () => (await queue()).items[0].text === "Second queued instruction",
    "queue order is persisted",
  );
  await poll(
    async () =>
      (await page.locator(".queued-item").first().textContent()).includes(
        "Second queued instruction",
      ),
    "queue UI applies server order",
  );
  await page
    .locator(".queued-item")
    .first()
    .getByRole("button", { name: "Cancel queued message", exact: true })
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
  await poll(
    async () => !(await page.locator("#send").isDisabled()),
    "failed fixture steer returns control",
  );
  assert.equal(
    await page.locator("#message").inputValue(),
    "Steering failure keeps this instruction",
  );
  assert.equal(
    (await queue()).items.length,
    1,
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
    "steer",
    "Enter uses after-tool-call delivery",
  );
  await poll(
    async () => !(await page.locator("#send").isDisabled()),
    "Enter failure returns control",
  );

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
    .getByRole("button", { name: "1 queued message", exact: true })
    .click();
  await page
    .locator(".queued-item")
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
