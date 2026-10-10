import { readTestState, spawnFixture as spawn, test } from "../playwright.mjs";
// Busy isolated runtime must not pull unchanged drafts once per second.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("draft idle poll browser", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const directory = await mkdtemp(join(tmpdir(), "studio-draft-idle-"));
  const fixture = spawn(
    "python3",
    [
      "-B",
      join(root, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      directory,
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let browser;
  let errorLog = "";
  fixture.stderr.on("data", (chunk) => {
    errorLog += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(errorLog)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const state = await readTestState(origin);
    const lead = state.threads.find((agent) => agent.name === "Release lead");
    browser = _browser;
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
      fixture.stdin.write(
        JSON.stringify({
          method: "fixture/agent-status",
          params: {
            agent: lead.id,
            status: index % 2 ? "waiting" : "completed",
          },
        }) + "\n",
      );
      await page.waitForTimeout(500);
    }
    await page.waitForTimeout(1500);
    const draftPulls = requests.filter(
      (path) => new URL(path, origin).searchParams.get("scope") === "drafts",
    ).length;
    const sessions = requests.filter((path) => path === "/api/session").length;
    assert.ok(draftPulls <= 1, `Unchanged drafts pulled ${draftPulls} times`);
    assert.ok(sessions <= 1, `Session polled ${sessions} times`);
    console.log(
      JSON.stringify({
        runtimeWrites: 12,
        intervalMs: 500,
        observationMs: 7500,
        draftPulls,
        sessions,
      }),
    );
  } finally {
    fixture.stdin?.end();
  }
});
