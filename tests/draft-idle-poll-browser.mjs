// Busy isolated runtime must not pull unchanged drafts once per second.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, "web/package.json"))("playwright-core");
const directory = await mkdtemp(join(tmpdir(), "studio-draft-idle-"));
const fixture = spawn("python3", ["-B", join(root, "tests/simple-ui-fixture.py"), directory],
  { stdio: ["pipe", "pipe", "pipe"] });
let browser;
let errorLog = "";
fixture.stderr.on("data", (chunk) => { errorLog += chunk; });
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) => resolve(Number(String(chunk).trim())));
    fixture.once("exit", () => reject(new Error(errorLog)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state?view=chat")).json();
  const lead = state.threads.find((agent) => agent.name === "Release lead");
  browser = await chromium.launch({ headless: true,
    executablePath: process.env.CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" });
  const page = await browser.newPage();
  const requests = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === "/api/session" || url.pathname === "/api/sync/pull")
      requests.push(url.pathname + url.search);
  });
  await page.goto(origin);
  await page.locator("#message").waitFor();
  await page.waitForTimeout(1500);
  requests.length = 0;
  for (let index = 0; index < 12; index++) {
    fixture.stdin.write(JSON.stringify({ method: "fixture/agent-status", params: {
      agent: lead.id, status: index % 2 ? "waiting" : "completed",
    } }) + "\n");
    await page.waitForTimeout(500);
  }
  await page.waitForTimeout(1500);
  const draftPulls = requests.filter((path) =>
    new URL(path, origin).searchParams.get("scope") === "drafts").length;
  const sessions = requests.filter((path) => path === "/api/session").length;
  assert.ok(draftPulls <= 1, `Unchanged drafts pulled ${draftPulls} times`);
  assert.ok(sessions <= 1, `Session polled ${sessions} times`);
  console.log(JSON.stringify({ runtimeWrites: 12, intervalMs: 500,
    observationMs: 7500, draftPulls, sessions }));
} finally {
  await browser?.close();
  fixture.stdin.end();
  if (fixture.exitCode === null)
    await new Promise((resolve) => {
      fixture.once("exit", resolve);
      setTimeout(() => { fixture.kill("SIGTERM"); resolve(); }, 3000).unref();
    });
}
