#!/usr/bin/env node
// Real runtime records, lost HTTP response, and responsive message navigation.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "studio-messages-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, MESSAGES_UI_FIXTURE: "1" },
  },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (data) => {
  log += data;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((a) => a.name === "Release lead");
  const records = state.runtime.complaints;
  assert.equal(records.length, 2);
  assert.equal(records.find((c) => c.author === lead.id).recipient, "user");
  assert.equal(records.find((c) => c.author !== lead.id).recipient, "lead");
  const leadMessage = records.find((c) => c.author === lead.id);
  const direct = state.runtime.rooms.find(
    (r) => r.kind === "private" && r.members.includes(lead.id),
  );
  const directName = state.threads.find(
    (agent) => direct.members.includes(agent.id) && agent.id !== lead.id,
  ).name;
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.locator("#message").fill("Keep my main conversation draft.");
  await page.locator("#messages-toggle").click();
  const drawer = page.getByRole("dialog", { name: "Messages", exact: true });
  await drawer.waitFor({ state: "visible" });
  assert.equal(
    await drawer
      .locator('[data-feed-item^="complaint:"] [data-complaint]')
      .count(),
    2,
  );
  assert.equal(await drawer.locator(".workspace-nav").count(), 0);
  await drawer.locator('[data-room="you"]').waitFor();
  assert.equal(await drawer.locator('[data-room="you"]').count(), 1);
  const roomSearch = drawer.getByRole("textbox", { name: "Search chats" });
  await roomSearch.fill("Team broadcast");
  await drawer.locator(`[data-room="broadcast:${lead.id}"]`).waitFor();
  assert.equal(
    await drawer.locator(`[data-room="broadcast:${lead.id}"]`).count(),
    1,
  );
  await roomSearch.fill(directName);
  await drawer.locator(`[data-room="${direct.id}"]`).waitFor();
  assert.equal(await drawer.locator(`[data-room="${direct.id}"]`).count(), 1);
  assert.equal(
    await page
      .locator("#open-complaints, #agent-chats-toggle, #canvas-toggle")
      .count(),
    0,
  );
  const message = drawer.locator(`[data-complaint="${leadMessage.id}"]`);
  await message.getByRole("button", { name: "Reply", exact: true }).waitFor();
  await message.getByRole("button", { name: "Reply", exact: true }).click();
  const reply = message.getByLabel("Reply", { exact: true });
  await reply.fill("The review account is ready.");
  const attempts = [];
  await page.route("**/api/complaints", async (route) => {
    attempts.push(route.request().postDataJSON());
    const response = await route.fetch();
    assert.equal(response.status(), 200);
    if (attempts.length === 1) await route.abort("failed");
    else await route.fulfill({ response });
  });
  await message
    .getByRole("button", { name: "Send reply", exact: true })
    .click();
  await message
    .getByRole("button", { name: "Retry response", exact: true })
    .waitFor();
  await drawer.locator(".mantine-Drawer-close").click();
  await page.locator("#messages-toggle").click();
  await drawer.waitFor({ state: "visible" });
  const reopenedMessage = drawer.locator(
    `[data-complaint="${leadMessage.id}"]`,
  );
  const replyToggle = reopenedMessage.getByRole("button", {
    name: /^(Reply|Close reply)$/,
  });
  await replyToggle.waitFor();
  if ((await replyToggle.innerText()) === "Reply") await replyToggle.click();
  await drawer
    .locator(`[data-complaint="${leadMessage.id}"]`)
    .getByRole("button", { name: "Retry response", exact: true })
    .click();
  await drawer
    .locator(`[data-complaint="${leadMessage.id}"]`)
    .getByRole("button", { name: "Send reply", exact: true })
    .waitFor();
  assert.equal(attempts.length, 2);
  assert.deepEqual(
    attempts[1],
    attempts[0],
    "retry preserves the request after drawer unmount",
  );
  const saved = await (
    await fetch(origin + "/api/complaint?id=" + leadMessage.id)
  ).json();
  assert.equal(saved.responses.length, 1);
  assert.equal(saved.responses[0].id, `user:${attempts[0].id}`);
  assert.equal(saved.responses[0].author, "user");
  const delivery = JSON.parse(
    execFileSync(
      "python3",
      [
        "-c",
        `import sqlite3,json,sys
c=sqlite3.connect(sys.argv[1]); c.row_factory=sqlite3.Row
print(json.dumps([dict(r) for r in c.execute("select * from runtime_events where kind='complaint_response'")]))`,
        join(root, "canvas.sqlite3"),
      ],
      { encoding: "utf8" },
    ),
  );
  assert.equal(delivery.length, 1);
  assert.equal(
    delivery[0].agent,
    leadMessage.author,
    "response reaches the lead who sent the message",
  );
  await roomSearch.fill(directName);
  await drawer.locator(`[data-room="${direct.id}"]`).click();
  await drawer
    .locator(".team-message")
    .filter({ hasText: "A private update before the final answer." })
    .waitFor();
  await drawer
    .getByRole("button", { name: "Earlier messages", exact: true })
    .click();
  await drawer
    .locator(".team-message")
    .filter({ hasText: "Earlier finding 0" })
    .waitFor();
  await drawer
    .getByRole("button", { name: "Latest messages", exact: true })
    .click();
  for (const width of [1920, 1280, 390, 320]) {
    await page.setViewportSize({ width, height: 900 });
    await page.waitForTimeout(500);
    if (!(await drawer.isVisible()))
      await page.locator("#messages-toggle").click();
    await drawer.waitFor({ state: "visible" });
    if (width < 700 && (await drawer.locator(".team-room-back").isVisible()))
      await drawer.getByRole("button", { name: "Back to chats" }).click();
    await page.waitForFunction(() => {
      const panel = document.querySelector(
        ".workspace-drawer .mantine-Drawer-content",
      );
      const rect = panel?.getBoundingClientRect();
      return (
        rect &&
        rect.width > 0 &&
        rect.left >= -1 &&
        rect.right <= innerWidth + 1
      );
    });
    await drawer.locator(".team-room-rows").evaluate((node) => {
      node.scrollTop = 0;
    });
    await page.screenshot({
      path: join(root, `messages-${width}.png`),
      animations: "disabled",
    });
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
      `page overflow at ${width}`,
    );
    const overflow = await drawer
      .locator(
        ".team-room-rows, .unified-message-scroll, .request-card, .request-copy, .message-inline-thread",
      )
      .evaluateAll((nodes) =>
        nodes
          .filter((n) => n.scrollWidth > n.clientWidth + 1)
          .map((n) => ({
            className: n.className,
            scroll: n.scrollWidth,
            client: n.clientWidth,
          })),
      );
    assert.deepEqual(overflow, [], `message overflow at ${width}`);
    if (width < 700) {
      await drawer.locator(`[data-room="${direct.id}"]`).click();
      await page.screenshot({
        path: join(root, `room-${width}.png`),
        animations: "disabled",
      });
      assert.ok(
        await drawer
          .locator(".team-room-detail")
          .evaluate((n) => n.scrollWidth <= n.clientWidth + 1),
      );
    }
  }
  await drawer.locator(".mantine-Drawer-close").click();
  await drawer.waitFor({ state: "hidden" });
  assert.equal(
    await page.locator("#message").inputValue(),
    "Keep my main conversation draft.",
  );
  assert.equal(
    await page.locator("#conversation-title").textContent(),
    lead.name,
  );
  await page.locator("#messages-toggle").click();
  await drawer.waitFor({ state: "visible" });
  if (await drawer.locator(".team-room-back").isVisible())
    await drawer.getByRole("button", { name: "Back to chats" }).click();
  await drawer.locator('[data-room="you"]').click();
  await drawer
    .locator(`[data-complaint="${leadMessage.id}"]`)
    .getByText("The review account is ready.", { exact: true })
    .waitFor();
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      evidence: root,
      cases: [
        "For you messages and team chat list",
        "lead messages to user and worker requests to lead",
        "real runtime response delivery",
        "lost response retry after drawer close",
        "pending and history",
        "room pagination",
        "mobile back",
        "main draft preserved",
        "responsive overflow",
      ],
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Evidence:", root, error);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
