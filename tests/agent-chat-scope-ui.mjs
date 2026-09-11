#!/usr/bin/env node
// Two lead conversations share a folder. Real room records and headless UI.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const project = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(project, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "studio-agent-chat-scope-"));
const fixture = spawn(
  "python3",
  ["-B", join(project, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, SCOPED_ROOMS_UI_FIXTURE: "1" },
  },
);
let log = "",
  browser;
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
  const other = state.threads.find((a) => a.name === "Other project");
  assert.equal(lead.cwd, other.cwd);
  const privateRoom = state.runtime.rooms.find(
    (r) => r.kind === "private" && r.members.includes(other.id),
  );
  assert.equal(
    privateRoom.rootId,
    undefined,
    "existing private rooms have no root metadata",
  );
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 900 },
  });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(origin);
  const dialog = page.getByRole("dialog", { name: "Messages", exact: true });
  const close = async () => {
    if (await dialog.isVisible()) await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
  };
  const select = async (id) => {
    await close();
    await page.locator(`[data-chat="${id}"]`).click();
    await page.locator("#messages-toggle").click();
    await dialog.waitFor();
    await dialog.getByRole("tab", { name: "Team", exact: true }).click();
  };
  const room = (id) => dialog.locator(`[data-room="${id}"]`);
  await select(other.id);
  assert.equal(await page.locator("#sidebar [data-room]").count(), 0);
  assert.equal(await page.locator("#sidebar [role=tab]").count(), 0);
  await room(`broadcast:${other.id}`).waitFor();
  assert.equal(await dialog.locator("[data-room]").count(), 1);
  assert.equal(await room(`broadcast:${lead.id}`).count(), 0);
  assert.equal(await room("broadcast:all").count(), 0);
  assert.equal(
    await room(privateRoom.id).count(),
    0,
    "cross-team room stays outside a single team's conversations",
  );
  await room(`broadcast:${other.id}`).click();
  await dialog
    .getByText("Only the second conversation", { exact: true })
    .last()
    .waitFor();
  await page.screenshot({ path: join(root, "second-chat.png") });
  await select(lead.id);
  assert.equal(await room(`broadcast:${other.id}`).count(), 0);
  assert.equal(await room("broadcast:all").count(), 0);
  assert.equal(
    await dialog.locator("[data-room]").count(),
    60,
    "bounded room list",
  );
  await dialog.getByRole("button", { name: "Show more chats" }).click();
  assert.ok((await dialog.locator("[data-room]").count()) > 60);
  const direct = state.runtime.rooms.find(
    (r) => r.kind === "private" && r.members.includes(lead.id),
  );
  await dialog
    .getByRole("textbox", { name: "Search team chats" })
    .fill(direct.name);
  await room(direct.id).click();
  await dialog
    .locator(".team-message")
    .filter({ hasText: "A private update before the final answer." })
    .waitFor();
  assert.equal(
    await page.locator("#conversation-title").textContent(),
    lead.name,
    "main conversation remains selected",
  );
  await dialog.getByRole("button", { name: "Earlier messages" }).click();
  await dialog
    .locator(".team-message")
    .filter({ hasText: "Earlier finding 0" })
    .waitFor();
  await dialog.getByRole("button", { name: "Latest messages" }).click();
  await page.screenshot({ path: join(root, "first-chat.png") });
  const overflow = await dialog.evaluate((el) =>
    [...el.querySelectorAll(".team-room-detail, .team-room-rows")].some(
      (node) => node.scrollWidth > node.clientWidth + 1,
    ),
  );
  assert.equal(overflow, false, "room layout contains long content");
  await close();
  await page.setViewportSize({ width: 600, height: 900 });
  await page.locator("#messages-toggle").click();
  await dialog.waitFor();
  await dialog.getByRole("tab", { name: "Team", exact: true }).click();
  await room(`broadcast:${lead.id}`).click();
  await dialog.getByRole("button", { name: "Back to team chats" }).click();
  await dialog.getByRole("textbox", { name: "Search team chats" }).waitFor();
  await page.screenshot({ path: join(root, "narrow-list.png") });
  await room(`broadcast:${lead.id}`).click();
  await page.screenshot({ path: join(root, "narrow-room.png") });
  await close();
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator("#tasks-toggle").click();
  await page.getByRole("dialog", { name: /Background tasks/ }).waitFor();
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      evidence: root,
      cases: [
        "per conversation in same project",
        "private room without rootId",
        "cross-team and global rooms excluded",
        "toolbar entry and intact main conversation",
        "room history and responsive navigation",
        "background drawer preserved",
        "global broadcast excluded",
        "bounded room list",
      ],
    }),
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
