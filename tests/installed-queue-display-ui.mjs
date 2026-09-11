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
  const post = async (text) => {
    const response = await fetch(origin + "/api/messages", {method: "POST", headers: {"Content-Type": "application/json", "Origin": origin, "X-Canvas-Token": initial.token}, body: JSON.stringify({id: crypto.randomUUID(),room:lead.id,text,delivery:"queue"})});
    assert.equal(response.status,200);
  };
  await post("Active fixture turn");
  await poll(async () => (await state()).threads.find(a => a.id===lead.id).inFlight, "fixture turn starts");
  for(const text of ["First queued instruction","Second queued instruction"]) await post(text);
  const queue = () =>
    fetch(`${origin}/api/queue?agent=${lead.id}`).then((response) =>
      response.json(),
    );
  assert.equal(
    await page.locator(".message-queue").count(),
    0,
    "No duplicate queue panel",
  );
  const queuedBubble = (text) =>
    page.locator(".message.user").filter({ hasText: text });
  await queuedBubble("First queued instruction").waitFor();
  assert.equal(
    await page.getByText("First queued instruction", { exact: true }).count(),
    1,
  );
  await page
    .locator(
      '.message.user:has(.inline-queue-actions[data-queue-position="1"])',
    )
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
    .locator(
      '.message.user:has(.inline-queue-actions[data-queue-position="2"])',
    )
    .getByRole("button", { name: "Move message first", exact: true })
    .click();
  await poll(
    async () => (await queue()).items[0].text === "Second queued instruction",
    "queue order is persisted",
  );
  await poll(
    async () =>
      (
        await page
          .locator(
            '.message.user:has(.inline-queue-actions[data-queue-position="1"])',
          )
          .textContent()
      ).includes("Second queued instruction"),
    "queue UI applies server order",
  );
  await page
    .locator(
      '.message.user:has(.inline-queue-actions[data-queue-position="1"])',
    )
    .getByRole("button", { name: "Cancel queued message", exact: true })
    .click();
  await poll(
    async () => (await queue()).items.length === 1,
    "queue cancellation is persisted",
  );
  await page.setViewportSize({width:390,height:844});
  await page.locator('.message.user').filter({hasText:'Revised first instruction'}).scrollIntoViewIfNeeded();
  assert.equal(await page.getByText('Revised first instruction',{exact:true}).count(),1);
  assert.equal(await page.evaluate(() => document.body.scrollWidth),390);
  await page.screenshot({path:join(root,'queue-mobile.png')});
  assert.deepEqual(errors,[]);
  console.log(JSON.stringify({result:'PASS',cases:['one message','edit','reorder','cancel','mobile'],evidence:root}));
} finally {
  if(browser) await browser.close();
  fixture.kill('SIGTERM');
}
