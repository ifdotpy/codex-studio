#!/usr/bin/env node
// Real app-server notification handlers, SQLite, event stream, and browser. No inference.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-stream-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser,
  page;
proc.stderr.on("data", (d) => (log += d));
const poll = async (fn, label) => {
  for (let i = 0; i < 100; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw Error(label + " " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = async () => await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
  const errors = [];
  let transcriptPolls = 0;
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("request", (r) => {
    if (r.url().includes("/api/transcript?id=")) transcriptPolls++;
  });
  // This suite verifies the native transcript transport without optional sync.
  await page.route("**/api/sync/identity", (r) =>
    r.fulfill({ status: 404, json: { error: "Unsupported sync" } }),
  );
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("Exercise the live stream");
  await page.locator("#send").click();
  let agent;
  await poll(async () => {
    agent = (await state()).runtime.agents.find(
      (a) => a.name === "Other project",
    );
    return agent.status === "running" && agent.turnId;
  }, "turn starts");
  const event = (method, params) =>
    proc.stdin.write(
      JSON.stringify({
        method,
        params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
      }) + "\n",
    );
  event("error", { willRetry: true, error: { message: "Reconnecting 1/5", codexErrorInfo: "other" } });
  await page.locator('[data-phase="retrying"]').waitFor();
  assert.equal(await page.locator(".native-error").count(), 0);
  event("item/agentMessage/delta", { itemId: "native-text", delta: "Connection restored" });
  await page.locator('[data-phase="writing"]').waitFor();
  event("modelProvider/authRecoveryStarted", { provider: "test", message: "Restoring sign-in" });
  await page.locator('[data-phase="auth"]').waitFor();
  event("modelProvider/authRecoveryCompleted", { provider: "test", message: "Sign-in restored" });
  await page.locator('[data-phase="thinking"]').waitFor();
  const error = { message: "Your session has expired.", codexErrorInfo: "unauthorized", additionalDetails: "x".repeat(8000) };
  event("error", { willRetry: false, error });
  await page.locator(".native-error").getByText("Your session has expired.", { exact: true }).waitFor();
  await page.locator(".native-error").getByText("Sign in again to this chat's account.", { exact: true }).waitFor();
  assert.equal(await page.locator(".native-error details").getAttribute("open"), null);
  event("turn/completed", { turn: { id: agent.turnId, status: "failed", error: null } });
  await page.locator('[data-phase="failed"]').waitFor();
  await page.locator(".native-notice.error").waitFor();
  assert.equal(await page.locator(".tool-card").filter({ hasText: "Your session has expired" }).count(), 0);
  await page.screenshot({ path: join(root, "native-error-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator(".native-error summary").click();
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  const box = await page.locator(".native-error pre").boundingBox();
  assert.ok(box.height <= 181, "error details have bounded height");
  await page.screenshot({ path: join(root, "native-error-mobile.png") });
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ passed: true, evidence: root }));
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
