import { readTestState, test, spawnFixture as spawn } from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Chat controls", async ({ page }) => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(
    new URL("../../../../../../", import.meta.url),
  );
  // Exercise production components against isolated HTTP and SQLite, with one transport failure.
  const skill = testRepo;
  const root = await mkdtemp(join(tmpdir(), "codex-chat-controls-ui-"));
  const fixture = spawn(
    "python3",
    [
      "-B",
      join(skill, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  fixture.stderr.on("data", (data) => (log += data));
  const poll = async (fn, label) => {
    for (let i = 0; i < 120; i++) {
      if (await fn()) return;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw Error(`${label}\n${log}`);
  };
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = () => readTestState(origin);
  await page.setViewportSize({ width: 1440, height: 960 });
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
  const openActions = async () => {
    await row().hover();
    try {
      await page.waitForFunction(() => {
        const button = document.querySelector(
          'button[aria-label="Actions for Other project"]',
        );
        if (!(button instanceof HTMLButtonElement)) return false;
        const style = getComputedStyle(button);
        if (style.opacity !== "1" || style.pointerEvents !== "auto")
          return false;
        const rect = button.getBoundingClientRect();
        const target = document.elementFromPoint(
          rect.x + rect.width / 2,
          rect.y + rect.height / 2,
        );
        return target === button || button.contains(target);
      });
    } catch (error) {
      const state = await page.evaluate(() => {
        const button = document.querySelector(
          'button[aria-label="Actions for Other project"]',
        );
        const chat = document.querySelector("[data-chat]");
        return {
          button: button?.outerHTML,
          buttonStyle: button && getComputedStyle(button).cssText,
          buttonOpacity: button && getComputedStyle(button).opacity,
          buttonPointerEvents: button && getComputedStyle(button).pointerEvents,
          row: chat?.parentElement?.parentElement?.outerHTML,
          hovered: document.querySelector(":hover")?.outerHTML,
        };
      });
      throw Error(
        `Row actions did not open: ${JSON.stringify(state)}. ${error}`,
      );
    }
    await actions().click();
    await page.waitForFunction(
      () =>
        document
          .querySelector('button[aria-label="Actions for Other project"]')
          ?.getAttribute("aria-expanded") === "true",
    );
    await page.getByRole("menu").waitFor({ state: "visible" });
  };
  await openActions();
  // The pin shows at once, before a slow entity pull arrives.
  const slowState = async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 3000));
    await route.continue().catch(() => {});
  };
  await page.route("**/api/sync/pull*", slowState);
  const pinnedAt = Date.now();
  await page.getByRole("menuitem", { name: "Pin", exact: true }).click();
  await page
    .locator(`[data-chat="${lead.id}"] .chat-pin`)
    .waitFor({ state: "visible", timeout: 1000 });
  assert.ok(Date.now() - pinnedAt < 1000, "pin shows before the entity pull");
  await page.unroute("**/api/sync/pull*", slowState);
  await poll(
    async () =>
      (await state()).threads.find((agent) => agent.id === lead.id).pinned,
    "pin is persisted",
  );
  const folder = join(root, "Release checks");
  await mkdir(folder);
  await openActions();
  await page
    .getByRole("menuitem", { name: "Change project directory", exact: true })
    .click();
  const folderDialog = page.getByRole("dialog", {
    name: "Choose project folder",
    exact: true,
  });
  await folderDialog.getByLabel("Folder path").fill(folder);
  await folderDialog
    .getByRole("button", { name: "Open path", exact: true })
    .click();
  await folderDialog
    .getByRole("button", { name: "Add project", exact: true })
    .click();
  await folderDialog.waitFor({ state: "hidden" });
  await poll(
    async () =>
      (await state()).threads
        .find((a) => a.id === lead.id)
        .cwd.endsWith("/Release checks"),
    "project directory is persisted",
  );
  await page.getByText("Release checks", { exact: true }).waitFor();
  await openActions();
  const archiveAction = page.getByRole("menuitem", {
    name: "Archive",
    exact: true,
  });
  try {
    await archiveAction.waitFor({ state: "visible" });
  } catch (error) {
    const menuText = await page.getByRole("menu").allTextContents();
    const thread = (await state()).threads.find((item) => item.id === lead.id);
    throw Error(
      `Archive action missing: ${JSON.stringify({ menuText, archived: thread?.archived, cwd: thread?.cwd, inFlight: thread?.inFlight })}. ${error}`,
    );
  }
  await archiveAction.click();
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
  await openActions();
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
  await page.waitForFunction(() => {
    const button = document.querySelector("#send");
    return button instanceof HTMLButtonElement && !button.disabled;
  });
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
  const queueAfterTurn = page.getByRole("button", {
    name: "Queue after turn",
    exact: true,
  });
  assert.equal(await queueAfterTurn.isVisible(), true);
  assert.equal(
    await queueAfterTurn.isDisabled(),
    true,
    "Queue after turn is unavailable while the composer is empty",
  );
  await page.locator("#message").fill("Immediate instruction");
  await page.locator("#message").press("Enter");
  await page
    .locator("#messages .message.user")
    .filter({ hasText: "Immediate instruction" })
    .waitFor();
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
  await page.locator("#message").press("Tab");
  assert.equal(
    (await queue()).items.some((item) => item.text === "Keyboard check"),
    false,
  );
  assert.equal(await page.locator("#message").inputValue(), "Keyboard check");
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
  const failedSteerText = "Steering failure keeps this instruction";
  let failedSteerId;
  const failSteerRequests = async (route) => {
    const body = route.request().postDataJSON();
    if (body?.text !== failedSteerText) return route.continue();
    if (failedSteerId) assert.equal(body.id, failedSteerId);
    else failedSteerId = body.id;
    return route.fulfill({
      status: 503,
      json: { error: "Fixture steer transport failure" },
    });
  };
  await page.route("**/api/messages", failSteerRequests);
  await page.locator("#send").click();
  const failedSteer = page
    .locator("#messages .message.user")
    .filter({ hasText: "Steering failure keeps this instruction" });
  await failedSteer.waitFor();
  await page
    .getByText("Fixture steer transport failure", { exact: true })
    .waitFor();
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
    (await queue()).items.some((item) => item.text === failedSteerText),
    false,
    "failed transport does not create a server queue row",
  );
  await page.unroute("**/api/messages", failSteerRequests);

  await page.locator("#message").fill("Keyboard instruction");
  await page.route(
    "**/api/messages",
    (r) =>
      r.fulfill({
        status: 503,
        json: { error: "Keyboard transport failure" },
      }),
    { times: 1 },
  );
  const entered = page.waitForRequest(
    (r) => r.url().endsWith("/api/messages") && r.method() === "POST",
  );
  await page.locator("#message").press("Enter");
  assert.equal(
    (await entered).postDataJSON().delivery,
    "after_tool",
    "Enter sends after-tool input",
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
    "Chat controls UI: PASS (attachment retry, draft reload, paste/drop, file preview, native send failure, quote, pin/archive/project, mobile)",
  );
  console.log(`Browser evidence: ${root}`);
});
