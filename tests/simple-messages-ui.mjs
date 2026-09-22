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
  const chats = page.locator(".unified-messages");
  await chats.waitFor();
  await chats.locator(".team-message").first().waitFor();
  assert.equal(await chats.getByRole("tab").count(), 0);
  assert.equal(await chats.locator("[data-room]").count(), 0);
  assert.equal(
    await chats.getByText("Command failures", { exact: true }).count(),
    0,
  );
  assert.ok(
    await chats.locator('[data-feed-item^="request:"]').count(),
    "For you questions share the feed",
  );
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
  await page.screenshot({ path: join(evidence, "messages-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(250);
  assert.ok(
    await chats.evaluate((node) => node.scrollWidth <= node.clientWidth),
  );
  await page.screenshot({ path: join(evidence, "messages-mobile.png") });
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ passed: true, evidence }));
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
