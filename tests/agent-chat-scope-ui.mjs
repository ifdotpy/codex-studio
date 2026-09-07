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
  const select = async (id) => {
    await page.getByRole("tab", { name: /^Chats/ }).click();
    await page.locator(`[data-chat="${id}"]`).click();
    await page.getByRole("tab", { name: /^Agent chats/ }).click();
  };
  const room = (id) => page.locator(`[data-room="${id}"]`);
  await select(other.id);
  await room(`broadcast:${other.id}`).waitFor();
  assert.equal(await page.locator("[data-room]").count(), 2);
  assert.equal(await room(`broadcast:${lead.id}`).count(), 0);
  assert.equal(await room("broadcast:all").count(), 0);
  await room(privateRoom.id).click();
  await page
    .getByRole("button", { name: `Back to ${other.name}`, exact: true })
    .waitFor();
  assert.equal(
    await page.locator("[data-room]").count(),
    2,
    "private room retains selected lead context",
  );
  await page.screenshot({ path: join(root, "second-chat.png") });
  await select(lead.id);
  assert.equal(await room(`broadcast:${other.id}`).count(), 0);
  assert.equal(await room("broadcast:all").count(), 0);
  assert.equal(
    await page.locator("[data-room]").count(),
    60,
    "large team list remains bounded",
  );
  await page.locator("#chat-search").fill(other.name);
  await room(privateRoom.id).click();
  await page
    .getByRole("button", { name: `Back to ${lead.name}`, exact: true })
    .waitFor();
  assert.equal(await room(`broadcast:${other.id}`).count(), 0);
  await page.locator("#chat-search").fill("");
  await page.screenshot({ path: join(root, "first-chat.png") });
  await page
    .getByRole("button", { name: `Back to ${lead.name}`, exact: true })
    .click();
  await page.locator(`#chat-list [data-chat="${lead.id}"]`).waitFor();
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      evidence: root,
      cases: [
        "per conversation in same project",
        "private room without rootId",
        "cross-team participant context",
        "global broadcast excluded",
        "bounded room list",
      ],
    }),
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
