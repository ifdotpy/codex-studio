// Render recorded worker and native failures in the production UI. No inference.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
const repo = new URL("../", import.meta.url).pathname;
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const dir = await mkdtemp(join(tmpdir(), "studio-tool-limit-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  log = "";
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await fetch(origin + "/api/state").then((r) => r.json());
  const lead = state.runtime.agents.find((a) => a.name === "Release lead");
  const initial = await fetch(origin + "/api/transcript?id=" + lead.id).then(
    (r) => r.json(),
  );
  const error = {
    message:
      "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 15th, 2026 3:23 AM.",
    codexErrorInfo: "usageLimitExceeded",
  };
  const worker = (id, turnId, failed = true) => ({
    id,
    role: "tool",
    title: "Worker result received",
    text: JSON.stringify({
      agent_id: id,
      name: "Worker " + id,
      status: failed ? "failed" : "completed",
      result: JSON.stringify(error),
    }),
    turnId,
    turnStatus: "failed",
  });
  const items = [
    {
      id: "atomic-limit",
      role: "tool",
      title: "Command finished",
      text: JSON.stringify({ status: "completed", exitCode: 0 }),
      turnId: "atomic-limit",
      turnStatus: "failed",
      turnError: error,
      turnErrorResolved: true,
    },
    {
      id: "legacy-limit",
      role: "tool",
      title: "Command finished",
      text: JSON.stringify({
        status: "completed",
        command: "cargo build",
        exitCode: 0,
      }),
      turnId: "legacy-limit",
      turnStatus: "failed",
    },
    {
      id: "foreign-limit",
      role: "tool",
      title: "Command finished",
      text: JSON.stringify({ status: "completed", exitCode: 0 }),
      turnId: "foreign-limit",
      turnStatus: "failed",
    },
    {
      id: "long-command",
      role: "tool",
      title: "Command finished",
      text: JSON.stringify({
        command:
          "cd /Users/igor/Projects/lumina/.worktrees/fieldview-lhs-native-wiring && CARGO_BUILD_JOBS=2 cargo build",
        status: "completed",
        exitCode: 0,
      }),
      turnId: "connection",
      turnStatus: "failed",
    },
    { id: "split-turn", role: "user", text: "Continue", turnId: "connection" },
    {
      id: "connection-error",
      role: "system",
      text: "Connection closed before the response completed.",
      nativeNotice: "error",
      nativeError: {
        message: "Connection closed before the response completed.",
        codexErrorInfo: "responseStreamDisconnected",
      },
      turnId: "connection",
      turnStatus: "failed",
    },
    worker("first", "one"),
    worker("second", "two"),
    worker("third", "two"),
    worker("fourth", "three"),
    worker("fifth", "three"),
    worker("sixth", "three"),
    {
      id: "own-limit",
      role: "system",
      text: error.message,
      nativeError: error,
      nativeNotice: "error",
      turnId: "own",
      turnStatus: "failed",
    },
    worker("quote", "quoted", false),
    {
      id: "latest-limit",
      role: "tool",
      title: "Command finished",
      text: JSON.stringify({ status: "completed", exitCode: 0 }),
      turnId: "latest-limit",
      turnStatus: "failed",
    },
  ];
  const transcript = {
    ...initial,
    items,
    order: items.map((i) => i.id),
    replace: true,
    agent: {
      ...initial.agent,
      status: "failed",
      inFlight: false,
      turnId: null,
      threadId: "history-thread",
      lastCompletedTurn: "latest-limit",
      error,
    },
  };
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 1000 },
  });
  page.setDefaultTimeout(12000);
  const errors = [];
  let errorRequests = 0;
  let failNextRead = false;
  let releaseErrors;
  const errorsReady = new Promise((resolve) => {
    releaseErrors = resolve;
  });
  await page.route("**/api/analytics?**", async (route) => {
    const params = new URL(route.request().url()).searchParams;
    assert.equal(params.get("view"), "turn-errors");
    assert.equal(params.get("agent"), lead.id);
    assert.ok(
      !params.get("turns").includes("atomic-limit"),
      "the transcript already supplies this reason",
    );
    errorRequests++;
    await errorsReady;
    if (failNextRead) {
      failNextRead = false;
      return route.fulfill({
        status: 503,
        json: { error: "Temporary read failure" },
      });
    }
    return route.fulfill({
      json: {
        turns: [
          {
            agentId: lead.id,
            threadId: "history-thread",
            turnId: "retry-limit",
            status: "failed",
            error,
          },
          {
            agentId: lead.id,
            threadId: "history-thread",
            turnId: "legacy-limit",
            status: "failed",
            error,
          },
          {
            agentId: lead.id,
            threadId: "other-thread",
            turnId: "foreign-limit",
            status: "failed",
            error,
          },
          {
            agentId: "other-agent",
            threadId: "history-thread",
            turnId: "foreign-limit",
            status: "failed",
            error,
          },
        ],
      },
    });
  });
  page.on("pageerror", (e) => errors.push(e.message));
  await page.route("**/api/sync/identity", (r) =>
    r.fulfill({ status: 404, json: { error: "Unsupported sync" } }),
  );
  await page.route("**/api/transcript**", (r) =>
    new URL(r.request().url()).searchParams.get("id") !== lead.id
      ? r.continue()
      : r.request().url().includes("/stream?")
        ? r.fulfill({
            contentType: "text/event-stream",
            body: "data: " + JSON.stringify(transcript) + "\n\n",
          })
        : r.fulfill({ json: transcript }),
  );
  await page.goto(origin);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const first = page.locator('[data-turn="one"] .tool-card');
  await first.waitFor({ state: "visible" });
  await page
    .locator('[data-turn="legacy-limit"] .turn-error-pending')
    .waitFor();
  assert.equal(
    await page
      .getByText("The error reason is not available in this history.", {
        exact: true,
      })
      .count(),
    0,
    "do not claim an absent reason while the read is pending",
  );
  await page
    .locator('[data-turn="atomic-limit"] .native-limit-notice')
    .waitFor();
  await page
    .locator('[data-turn="latest-limit"] .native-limit-notice')
    .waitFor();
  await page.locator('[data-turn="legacy-limit"]').scrollIntoViewIfNeeded();
  for (const width of [1280, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    await page.screenshot({
      path: join(dir, `error-details-pending-${width}.png`),
    });
  }
  await page.setViewportSize({ width: 1280, height: 1000 });
  releaseErrors();
  for (const turn of ["legacy-limit", "latest-limit"]) {
    const notice = page.locator(`[data-turn="${turn}"] .native-limit-notice`);
    await notice.getByText("Usage limit reached", { exact: true }).waitFor();
    assert.match(await notice.innerText(), /Sep 15th, 2026 3:23 AM/);
  }
  assert.match(
    await page.locator('[data-turn="foreign-limit"]').innerText(),
    /The error reason is not available/,
  );
  assert.equal(
    await page
      .locator('[data-turn="foreign-limit"] .native-limit-notice')
      .count(),
    0,
    "another thread or agent cannot supply this turn's error",
  );
  assert.equal(errorRequests, 1, "one read recovers all missing turn errors");
  assert.equal(
    await page.locator('.agent-phase[data-phase="failed"]').count(),
    0,
    "do not repeat the terminal notice as Failed",
  );
  assert.match(await first.innerText(), /Worker stopped: usage limit reached/);
  assert.match(await first.innerText(), /Sep 15th, 2026 3:23 AM/);
  assert.equal(
    await first.getAttribute("open"),
    null,
    "raw output remains closed",
  );
  assert.equal(await page.locator('[data-turn="two"] .tool-card').count(), 2);
  const three = page.locator('[data-turn="three"] .turn-work');
  assert.equal(await three.getAttribute("open"), null);
  assert.match(await three.innerText(), /Account limit reached/);
  const own = page.locator('[data-turn="own"]');
  await own.getByText("Usage limit reached", { exact: true }).waitFor();
  assert.match(await own.innerText(), /Sep 15th, 2026 3:23 AM/);
  assert.equal(
    await own.locator(".turn-problem").count(),
    0,
    "native limit notice replaces generic failure footer",
  );
  assert.doesNotMatch(
    await page.locator('[data-turn="quoted"] .tool-card').innerText(),
    /Worker stopped/,
    "successful quoted errors do not become failures",
  );
  await first.locator(":scope > summary").click();
  await first
    .locator(".tool-output")
    .filter({ hasText: error.message })
    .waitFor();
  await first.locator(":scope > summary").click();
  for (const width of [1280, 390, 320]) {
    await page.setViewportSize({ width, height: 1000 });
    const command = page.locator('[data-message="long-command"]');
    await command.scrollIntoViewIfNeeded();
    assert.match(
      await page.locator('[data-turn="connection"]').first().innerText(),
      /Connection closed before the response completed/,
    );
    const title = command.locator(".tool-title > span");
    assert.equal(
      await command
        .locator(".tool-title")
        .evaluate((el) => getComputedStyle(el).display),
      "block",
    );
    assert.equal(await title.innerText(), "Command finished");
    assert.equal(
      await title.evaluate((el) => getComputedStyle(el).whiteSpace),
      "normal",
    );
    assert.equal(
      await title.evaluate((el) => el.scrollWidth <= el.clientWidth),
      true,
    );
    assert.equal(
      await first
        .locator(".activity-limit")
        .evaluate((el) => getComputedStyle(el).whiteSpace),
      "normal",
      "limit reason wraps instead of being truncated",
    );
    await page.screenshot({
      path: join(dir, `limit-history-${width}.png`),
      animations: "disabled",
    });
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
      "no horizontal overflow",
    );
  }
  failNextRead = true;
  transcript.items = [
    {
      id: "retry-limit",
      role: "tool",
      title: "Command finished",
      text: JSON.stringify({ status: "completed", exitCode: 0 }),
      turnId: "retry-limit",
      turnStatus: "failed",
    },
  ];
  transcript.order = ["retry-limit"];
  transcript.agent.error = null;
  transcript.agent.lastCompletedTurn = null;
  await page.reload();
  const retryTurn = page.locator('[data-turn="retry-limit"]');
  await retryTurn
    .getByText("Could not load error details.", { exact: true })
    .waitFor();
  assert.equal(
    await retryTurn
      .getByText("The error reason is not available in this history.", {
        exact: true,
      })
      .count(),
    0,
  );
  await retryTurn.getByRole("button", { name: "Retry", exact: true }).click();
  await retryTurn.getByText("Usage limit reached", { exact: true }).waitFor();
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, evidence: dir }));
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
