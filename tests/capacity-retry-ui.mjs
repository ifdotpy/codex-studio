#!/usr/bin/env node
// Real runtime, SQLite and two browser pages. The native server is a fixture.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const rootDir = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(rootDir, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "studio-capacity-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(rootDir, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CAPACITY_UI_FIXTURE: "1" },
  },
);
let log = "",
  browser;
fixture.stderr.on("data", (data) => (log += data));
const until = async (fn, name, timeout = 16000) => {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`${name}: ${log}`);
};
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = () => fetch(`${origin}/api/state`).then((r) => r.json());
  const snapshot = await state();
  const id = snapshot.runtime.agents.find((a) => a.name === "Other project").id;
  const agent = async () =>
    (await state()).runtime.agents.find((a) => a.id === id);
  let threadId;
  const starts = async () =>
    (await readFile(join(root, "capacity-starts.jsonl"), "utf8"))
      .trim()
      .split("\n")
      .filter(Boolean)
      .map(JSON.parse)
      .filter((entry) => !threadId || entry.threadId === threadId);
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const context = await browser.newContext({
    viewport: { width: 1200, height: 900 },
  });
  const page = await context.newPage(),
    other = await context.newPage();
  const errors = [];
  for (const p of [page, other]) {
    p.on("pageerror", (error) => errors.push(error.message));
    await p.goto(origin);
    await p.locator(`[data-chat="${id}"]`).click();
  }
  await page
    .locator("#message")
    .fill("Keep the original user message exactly once.");
  await page.locator("#send").click();
  await until(async () => (await agent()).turnId, "original turn");
  const first = await agent();
  threadId = first.threadId;
  const notifyFailure = (a) =>
    fixture.stdin.write(
      JSON.stringify({
        method: "turn/completed",
        params: {
          threadId: a.threadId,
          turn: {
            id: a.turnId,
            status: "failed",
            error: {
              message: "Model is at capacity",
              codexErrorInfo: "serverOverloaded",
            },
          },
        },
      }) + "\n",
    );
  notifyFailure(first);
  await page
    .locator('.capacity-retry[data-retry-status="scheduled"]')
    .waitFor();
  await page.locator("#message").fill("Keep this draft while retrying.");
  await page.setViewportSize({ width: 320, height: 740 });
  await page
    .getByRole("button", { name: "Cancel automatic retry", exact: true })
    .click();
  await page.getByText("Automatic retry cancelled.", { exact: true }).waitFor();
  await other
    .getByText("Automatic retry cancelled.", { exact: true })
    .waitFor();
  await page.reload();
  await page.getByText("Automatic retry cancelled.", { exact: true }).waitFor();
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 320);
  await page.screenshot({ path: join(root, "capacity-cancel-mobile.png") });
  await new Promise((resolve) => setTimeout(resolve, 10500));
  assert.equal(
    (await starts()).length,
    1,
    "cancel survives reload and the original deadline",
  );
  const retryId = (await agent()).capacityRetry.id;
  let lost = false;
  await page.route("**/api/capacity-retry", async (route) => {
    const response = await route.fetch();
    assert.equal(response.status(), 200);
    lost = true;
    await route.abort("failed");
  });
  await page.getByRole("button", { name: "Retry now", exact: true }).click();
  await until(
    async () =>
      (await agent()).turnId && (await agent()).turnId !== first.turnId,
    "manual empty turn",
  );
  assert.ok(lost);
  const duplicate = await fetch(`${origin}/api/capacity-retry`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": (await state()).token,
    },
    body: JSON.stringify({ id, retry_id: retryId, action: "retry" }),
  });
  assert.equal(duplicate.status, 200);
  assert.equal(
    (await starts()).length,
    2,
    "lost response and duplicate action produce one native turn",
  );
  assert.deepEqual((await starts())[1].input, []);
  assert.equal(
    await page.locator("#message").inputValue(),
    "Keep this draft while retrying.",
  );
  await page.unroute("**/api/capacity-retry");
  const second = await agent();
  notifyFailure(second);
  await until(
    async () => (await agent()).capacityRetry.status === "scheduled",
    "next retry timer",
  );
  assert.equal(
    Math.round(
      (await agent()).capacityRetry.dueAt -
        (await agent()).capacityRetry.updatedAt,
    ),
    30,
  );
  // New user input resets the schedule to the actual ten-second delay.
  await page
    .getByRole("button", { name: "Cancel automatic retry", exact: true })
    .click();
  await page.getByText("Automatic retry cancelled.", { exact: true }).waitFor();
  await page
    .locator("#message")
    .fill("A new instruction resets the retry schedule.");
  await page.locator("#send").click();
  await until(
    async () =>
      (await agent()).turnId && (await agent()).turnId !== second.turnId,
    "new user turn",
  );
  const third = await agent();
  notifyFailure(third);
  await page
    .locator('.capacity-retry[data-retry-status="scheduled"]')
    .waitFor();
  const before = (await starts()).length;
  await until(
    async () => (await starts()).length === before + 1,
    "automatic empty retry",
    16000,
  );
  assert.deepEqual((await starts()).at(-1).input, []);
  assert.equal(
    (await starts()).length,
    4,
    "exactly two user turns and two empty retries",
  );
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ passed: true, evidence: root }));
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
