#!/usr/bin/env node
// Production bundle, isolated backend, no model calls.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-team-panel-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  log = "";
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);

  const lead = snapshot.threads.find((agent) => agent.name === "Release lead");
  const room = snapshot.runtime.rooms.find(
    (room) => room.kind === "private" && room.members.includes(lead.id),
  );
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  assert.equal(await page.locator("#sidebar .agent-avatar").count(), 0);
  assert.equal(await page.locator("#sidebar .sidebar-team-chats").count(), 0);
  const title = await page.locator("#conversation-title").innerText();
  await page.locator("#messages-toggle").click();
  const chats = page.locator(".messages-chat-list");
  await chats.waitFor();
  await chats.locator('[data-room="you"]').waitFor();
  assert.equal(await chats.getByRole("tab").count(), 0);
  assert.ok((await chats.locator("[data-room]").count()) > 1);
  assert.ok((await chats.locator(".team-room-row svg").count()) > 1);
  assert.equal(
    await chats.getByText("Command failures", { exact: true }).count(),
    0,
  );
  assert.ok(
    await chats.locator('[data-feed-item^="request:"]').count(),
    "For you questions share the feed",
  );
  await chats.getByRole("textbox", { name: "Search chats" }).fill("Worker 39");
  await chats.locator(`[data-room="${room.id}"]`).click();
  await chats.locator(".team-message").first().waitFor();
  assert.equal(await chats.locator('[data-feed-item^="request:"]').count(), 0);
  assert.equal(
    await chats
      .locator(`[data-room="${room.id}"]`)
      .getAttribute("aria-pressed"),
    "true",
  );
  const roomData = await (
    await fetch(origin + "/api/agent-chat?room=" + encodeURIComponent(room.id))
  ).json();
  assert.ok(roomData.messages.length > 0);
  const renderedIds = await chats
    .locator("[data-message]")
    .evaluateAll((nodes) => nodes.map((node) => node.dataset.message));
  assert.deepEqual(
    renderedIds,
    roomData.messages.map((message) => message.id),
  );
  await chats.getByRole("textbox", { name: "Search chats" }).fill("");
  await chats.locator('[data-room="you"]').click();
  assert.equal(await chats.locator(".team-message").count(), 0);
  assert.equal(
    await chats.getByRole("button", { name: "Earlier messages" }).count(),
    0,
  );
  assert.ok(await chats.locator('[data-feed-item^="request:"]').count());
  await chats.getByRole("textbox", { name: "Search chats" }).fill("Worker 39");
  await chats.locator(`[data-room="${room.id}"]`).click();
  await chats.locator(".team-message").first().waitFor();
  const another = snapshot.runtime.rooms.find(
    (item) => item.kind === "broadcast" && item.rootId === lead.id,
  );
  await chats
    .getByRole("textbox", { name: "Search chats" })
    .fill("Team broadcast");
  await chats.locator(`[data-room="${another.id}"]`).click();
  await chats.getByRole("textbox", { name: "Search chats" }).fill("Worker 39");
  await chats.locator(`[data-room="${room.id}"]`).click();
  await chats.locator(".team-message").first().waitFor();
  assert.deepEqual(
    await chats
      .locator("[data-message]")
      .evaluateAll((nodes) => nodes.map((node) => node.dataset.message)),
    renderedIds,
  );
  await chats.getByRole("textbox", { name: "Search chats" }).fill("");
  const pageData = await (
    await fetch(
      origin + "/api/agent-chat?room=" + encodeURIComponent("feed:" + lead.id),
    )
  ).json();
  assert.ok(
    new Set(pageData.messages.map((message) => message.room)).size > 1,
    "feed includes multiple rooms",
  );
  const seq = pageData.messages.map((message) => message.seq);
  assert.deepEqual(
    seq,
    [...seq].sort((a, b) => a - b),
  );
  assert.equal(await page.locator("#conversation-title").innerText(), title);
  assert.equal(await page.getByRole("dialog").count(), 1);
  await page.waitForTimeout(250);
  await chats.locator(".team-room-rows").evaluate((node) => {
    node.scrollTop = 0;
  });
  await page.screenshot({ path: join(evidence, "messages-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(250);
  assert.ok(
    await chats.evaluate((node) => node.scrollWidth <= node.clientWidth),
  );
  await page.screenshot({ path: join(evidence, "messages-mobile.png") });
  await chats.getByRole("button", { name: "Back to chats" }).click();
  assert.equal(await chats.locator(".team-room-detail").isVisible(), false);
  await chats.getByRole("textbox", { name: "Search chats" }).fill("For you");
  assert.equal(await chats.locator("[data-room]").count(), 1);
  await chats.getByRole("textbox", { name: "Search chats" }).fill("");
  await page.screenshot({ path: join(evidence, "messages-mobile-list.png") });
  await chats.locator('[data-room="you"]').click();
  assert.ok(await chats.locator('[data-feed-item^="request:"]').count());
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ passed: true, evidence }));
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
