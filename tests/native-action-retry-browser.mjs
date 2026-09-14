#!/usr/bin/env node
// Production UI and HTTP handlers with fake native commands in a hidden browser.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-action-retry-"));
const fixture = spawn(
  process.env.PYTHON || "/opt/homebrew/bin/python3",
  ["-B", join(repo, "tests/native-action-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  log = "";
fixture.stderr.on("data", (chunk) => (log += chunk));
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(origin + "/api/state")).json();
  const lead = snapshot.threads.find((a) => a.name === "Action lead");
  const post = (body) =>
    fetch(origin + "/api/action", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": snapshot.token,
      },
      body: JSON.stringify(body),
    });
  const legacy = await post({ id: lead.id, action: "review" });
  assert.equal(legacy.status, 400);
  assert.match((await legacy.json()).error, /Reload Studio/);
  const calls = async () =>
    (await readFile(join(evidence, "native-actions.jsonl"), "utf8"))
      .trim()
      .split("\n")
      .map(JSON.parse);
  browser =
    process.env.BROWSER === "webkit"
      ? await webkit.launch({ headless: true })
      : await chromium.launch({
          headless: true,
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        });
  const page = await browser.newPage({
    // Keep HTTP failure injection on page routes after reload.
    serviceWorkers: "block",
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
    userAgent:
      "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1",
  });
  page.setDefaultTimeout(12000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const requests = [];
  let loseReply = true;
  await page.route("**/api/action", async (route) => {
    const request = route.request().postDataJSON();
    requests.push(request);
    const stored = await page.evaluate(
      (id) => localStorage.getItem(id),
      `studio-native-action:${snapshot.stateDir}:${lead.id}`,
    );
    assert.equal(JSON.parse(stored).request_id, request.request_id);
    const response = await route.fetch();
    if (loseReply) {
      loseReply = false;
      await route.abort("failed");
    } else await route.fulfill({ response });
  });
  await page.goto(origin);
  await page.locator("#message").waitFor();
  if ((await page.locator("#conversation-title").textContent()) !== lead.name) {
    const chat = page.locator(`[data-chat="${lead.id}"]`);
    if (!(await chat.isVisible()))
      await page.getByLabel("Toggle conversations").click();
    await chat.click();
  }
  await page.locator("#sidebar").waitFor({ state: "hidden" });
  for (const action of ["review", "compact"]) {
    loseReply = true;
    await page.locator("#message").fill("/" + action);
    const lost = page.waitForEvent(
      "requestfailed",
      (request) => request.url() === origin + "/api/action",
    );
    await page.locator("#send").click();
    await lost;
    await page.getByRole("button", { name: "Check action request" }).waitFor();
    await page.waitForFunction(() => {
      const button = document.querySelector(".native-action-pending button");
      return button && !button.disabled;
    });
    await page.reload();
    await page.getByRole("button", { name: "Check action request" }).waitFor();
    await page.screenshot({
      path: join(evidence, `${action}-pending-mobile.png`),
    });
    await page.locator("#sidebar").waitFor({ state: "hidden" });
    const box = await page.locator(".native-action-pending").boundingBox();
    assert.ok(box.width >= 350 && box.x >= 0 && box.x + box.width <= 391);
    const buttonFits = await page
      .getByRole("button", { name: "Check action request" })
      .evaluate((button) => {
        const label = button.querySelector(".mantine-Button-label");
        const text = document.createRange();
        text.selectNodeContents(label);
        const bounds = button.getBoundingClientRect();
        const content = text.getBoundingClientRect();
        return (
          content.width > 0 &&
          content.left >= bounds.left &&
          content.right <= bounds.right &&
          content.right <= window.innerWidth &&
          label.scrollWidth <= label.clientWidth
        );
      });
    assert.ok(
      buttonFits,
      "The complete retry button text fits in the mobile chat",
    );
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    );
    await page.getByRole("button", { name: "Check action request" }).click();
    await page.locator(".native-action-pending").waitFor({ state: "hidden" });
    await page.waitForFunction(
      () => document.querySelector("#message")?.value === "",
    );
    const pair = requests.slice(-2);
    assert.deepEqual(pair[0], pair[1]);
    const method =
      action === "review" ? "review/start" : "thread/compact/start";
    assert.equal((await calls()).filter((c) => c.method === method).length, 1);
    const conflict = await post({
      ...pair[0],
      action: action === "review" ? "compact" : "review",
    });
    assert.equal(conflict.status, 400);
    await page.locator("#message").fill("/" + action);
    await page.locator("#send").click();
    await page.waitForFunction(
      () => document.querySelector("#message")?.value === "",
    );
    assert.notEqual(requests.at(-1).request_id, pair[0].request_id);
    assert.equal((await calls()).filter((c) => c.method === method).length, 2);
  }
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator('[data-action="review"]').click();
  await page.locator(".native-action-pending").waitFor({ state: "hidden" });
  assert.equal(
    (await calls()).filter((c) => c.method === "review/start").length,
    3,
  );
  const beforeStorageFailure = requests.length;
  await page.evaluate(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith("studio-native-action:"))
        throw new Error("Fixture storage unavailable");
      return original.call(this, key, value);
    };
  });
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator('[data-action="compact"]').click();
  await page
    .getByRole("status")
    .filter({ hasText: "Fixture storage unavailable" })
    .waitFor();
  assert.equal(requests.length, beforeStorageFailure);
  const configured = await fetch(origin + "/api/configure", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": snapshot.token,
    },
    body: JSON.stringify({ id: lead.id, tokenBudget: 1 }),
  });
  assert.equal(configured.status, 200);
  const beforeBudget = (await calls()).length;
  for (const action of ["review", "compact"]) {
    const rejected = await post({
      id: lead.id,
      action,
      request_id: "budget-" + action,
    });
    assert.equal(rejected.status, 400);
    assert.match((await rejected.json()).error, /budget/);
  }
  const replayAtBudget = await post(requests[0]);
  assert.equal(replayAtBudget.status, 200);
  assert.equal((await replayAtBudget.json()).replayed, true);
  assert.equal((await calls()).length, beforeBudget);
  assert.deepEqual(errors, []);
  console.log(
    `PASS ${process.env.BROWSER || "chromium"} iPhone emulation, native action lost reply, reload, exact retry, changed input, legacy rejection, distinct actions, menu action, storage refusal, budget admission. Screenshots: ${evidence}`,
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
