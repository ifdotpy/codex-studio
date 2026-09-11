#!/usr/bin/env node
// Real React with a fake recovery API. No backend, credentials, or model calls.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";

const root = resolve(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { createServer } = await import(require.resolve("vite"));
const { chromium } = require("playwright-core");
const evidence = await mkdtemp(join(tmpdir(), "studio-disconnect-recovery-"));
const disconnected =
  "Codex disconnected. Review the transcript before resuming.";
const restarted =
  "Server restarted during a turn. Review history, then send a new instruction.";
const original = {
  id: "first",
  source: "managed",
  status: "interrupted",
  inFlight: false,
  threadId: "thread-first",
  turnId: "turn-first",
  epoch: 1,
  accountKey: "default",
  error: disconnected,
};
const entry = root + "/disconnect-recovery-entry.tsx";
const server = await createServer({
  configFile: false,
  root,
  cacheDir: join(evidence, "vite"),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "disconnect-recovery-fixture",
      configureServer(server) {
        server.middlewares.use(
          "/disconnect-recovery-test",
          (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<html><body><div id="root"></div><script type="module" src="/disconnect-recovery-entry.tsx"></script></body></html>',
            );
          },
        );
      },
      resolveId(id) {
        if (id === "/disconnect-recovery-entry.tsx") return entry;
      },
      load(id) {
        if (id !== entry) return;
        return `
import React, {useState} from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import {NativeError} from '/src/components/NativeNotice.tsx';
import {setToken} from '/src/api.ts';
setToken('fixture-token');
window.refreshes = [];
function Harness() {
  const [agent, setAgent] = useState(() => JSON.parse(localStorage.getItem('fixture-agent') || 'null') || ${JSON.stringify(original)});
  window.setAgent = setAgent;
  return <MantineProvider><div data-agent={agent.id}>
    <NativeError agent={agent} refresh={async () => {window.refreshes.push(agent.id);}} />
    <p data-history>Previously recorded conversation remains available.</p>
  </div></MantineProvider>;
}
createRoot(document.getElementById('root')).render(<Harness/>);`;
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage();
  const errors = [],
    requests = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let reply = { status: "unconfirmed" },
    held;
  let hold = false,
    fail = false;
  await page.route("**/api/**", async (route) => {
    const request = route.request();
    requests.push({
      path: new URL(request.url()).pathname,
      method: request.method(),
      body: request.postDataJSON(),
      token: request.headers()["x-canvas-token"],
    });
    if (hold) {
      held = route;
      return;
    }
    await route.fulfill(
      fail
        ? { status: 503, json: { error: "Recovery service unavailable" } }
        : { json: reply },
    );
  });
  await page.goto(server.resolvedUrls.local[0] + "disconnect-recovery-test");
  const button = page.getByRole("button", {
    name: "Check connection",
    exact: true,
  });
  await button.waitFor();
  const history = await page.locator("[data-history]").elementHandle();
  assert.equal(
    requests.length,
    0,
    "mounting the notice does not start recovery",
  );
  hold = true;
  await button.evaluate((node) => {
    node.click();
    node.click();
  });
  await page.waitForFunction(() => document.querySelector("button")?.disabled);
  for (let attempt = 0; !held && attempt < 100; attempt++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(held);
  assert.equal(
    requests.length,
    1,
    "duplicate click cannot start another recovery check",
  );
  await held.fulfill({ json: reply });
  hold = false;
  held = undefined;
  await page
    .getByRole("status")
    .filter({ hasText: "previous outcome remains unconfirmed" })
    .waitFor();
  assert.match(
    await page.locator(".native-error").innerText(),
    /Codex disconnected/,
  );
  assert.equal(await history.evaluate((node) => node.isConnected), true);
  assert.deepEqual(await page.evaluate(() => window.refreshes), []);

  for (const outcome of ["completed", "failed", "interrupted"]) {
    reply = { status: "reconciled", outcome };
    await button.click();
    await page.waitForFunction(
      (count) => window.refreshes.length === count,
      ["completed", "failed", "interrupted"].indexOf(outcome) + 1,
    );
  }
  reply = { status: "superseded" };
  await button.click();
  await page
    .getByRole("status")
    .filter({ hasText: "conversation changed" })
    .waitFor();
  assert.equal(await page.evaluate(() => window.refreshes.length), 3);
  fail = true;
  await button.click();
  await page
    .getByRole("status")
    .filter({ hasText: "Recovery service unavailable" })
    .waitFor();
  fail = false;
  assert.match(
    await page.locator(".native-error").innerText(),
    /Codex disconnected/,
  );

  for (const patch of [
    { status: "running" },
    { inFlight: true },
    { threadId: null },
    { turnId: null },
    { error: "Codex disconnected." },
    { error: "An unrelated interruption" },
    { error: { message: disconnected } },
    { source: "imported" },
    { autoWake: true },
    { startAttempt: { id: "starting" } },
    { accountTransferId: "transfer" },
    { workspaceOperation: "restore" },
    { deletedAt: 1 },
    {
      nativeThreadBlock: {
        threadId: "thread-first",
        error: { codexErrorInfo: "misalignmentPolicyViolation" },
      },
    },
  ]) {
    await page.evaluate((agent) => window.setAgent(agent), {
      ...original,
      ...patch,
    });
    await button.waitFor({ state: "hidden" });
  }
  await page.evaluate((agent) => window.setAgent(agent), {
    ...original,
    error: restarted,
  });
  await button.waitFor();
  reply = {
    status: "unconfirmed",
    error: "Native history remains unavailable.",
  };
  await button.click();
  await page
    .getByRole("status")
    .filter({ hasText: "Native history remains unavailable." })
    .waitFor();
  assert.doesNotMatch(
    await page.getByRole("status").innerText(),
    /Connection checked/,
    "a failed transport or history read cannot claim a successful connection check",
  );
  assert.match(
    await page.locator(".native-error").innerText(),
    /Server restarted during a turn/,
  );

  hold = true;
  await button.click();
  for (let attempt = 0; !held && attempt < 100; attempt++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.ok(held);
  await page.evaluate((agent) => window.setAgent(agent), {
    ...original,
    id: "second",
    threadId: "thread-second",
    turnId: "turn-second",
  });
  await page.locator('[data-agent="second"]').waitFor();
  await held.fulfill({ json: { status: "reconciled", outcome: "completed" } });
  await page.evaluate(
    () =>
      new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      ),
  );
  assert.deepEqual(
    await page.evaluate(() => window.refreshes),
    ["first", "first", "first"],
    "an old chat response cannot refresh a different chat",
  );
  assert.equal(
    await page.getByRole("status").count(),
    0,
    "old recovery text cannot leak into another chat",
  );
  assert.equal(await button.isEnabled(), true);
  hold = false;
  held = undefined;
  reply = { status: "unconfirmed", checked: true };
  await page.evaluate((agent) => window.setAgent(agent), original);
  await button.click();
  await page.waitForFunction(() => window.refreshes.length === 4);
  const checkedAgent = {
    ...original,
    connectionCheck: {
      at: Date.now() / 1000,
      epoch: original.epoch,
      accountKey: original.accountKey,
      threadId: original.threadId,
      turnId: original.turnId,
      previousError: original.error,
      nativeState: "idle",
    },
  };
  await page.evaluate((agent) => {
    localStorage.setItem("fixture-agent", JSON.stringify(agent));
    window.setAgent(agent);
  }, checkedAgent);
  const reviewTitle = page.getByText("Previous turn needs review", {
    exact: true,
  });
  await reviewTitle.waitFor();
  await page
    .getByText(
      "Codex answered the connection check. The previous outcome is unconfirmed. Review the history before continuing.",
      { exact: true },
    )
    .waitFor();
  assert.doesNotMatch(
    await page.locator(".native-error > span").innerText(),
    /disconnected/,
  );
  await page.locator(".native-error details > summary").click();
  assert.equal(
    await page.locator(".native-error details pre").innerText(),
    disconnected,
  );
  const beforeReload = requests.length;
  await page.reload();
  await reviewTitle.waitFor();
  assert.equal(
    requests.length,
    beforeReload,
    "a saved receipt restores presentation without another check",
  );
  await button.waitFor();
  for (const patch of [
    { accountKey: "other" },
    { epoch: 2 },
    { threadId: "other-thread" },
    { turnId: "other-turn" },
    { error: restarted },
    { status: "completed" },
    {
      connectionCheck: {
        ...checkedAgent.connectionCheck,
        previousError: restarted,
      },
    },
    { connectionCheck: { ...checkedAgent.connectionCheck, at: 0 } },
  ]) {
    await page.evaluate((agent) => window.setAgent(agent), {
      ...checkedAgent,
      ...patch,
    });
    await reviewTitle.waitFor({ state: "hidden" });
  }
  await page.evaluate((agent) => window.setAgent(agent), {
    ...checkedAgent,
    connectionCheck: { ...checkedAgent.connectionCheck, nativeState: "active" },
  });
  await reviewTitle.waitFor();
  await page
    .getByText(
      "Codex reported this thread as active. Its outcome is not confirmed.",
      { exact: true },
    )
    .waitFor();
  assert.equal(
    await page.locator("[data-history]").innerText(),
    "Previously recorded conversation remains available.",
  );
  assert.ok(
    requests.every(
      (request) =>
        request.path === "/api/connection-recovery" &&
        request.method === "POST" &&
        request.token === "fixture-token",
    ),
  );
  assert.ok(
    requests.every(
      (request) =>
        JSON.stringify(request.body) === JSON.stringify({ id: "first" }),
    ),
    "the only mutation checks connection; no prompt is sent",
  );
  assert.deepEqual(errors, []);
  await page.screenshot({ path: join(evidence, "disconnect-recovery.png") });
  console.log(
    JSON.stringify({
      result: "PASS",
      cases: [
        "exact eligibility",
        "explicit click",
        "duplicate lock",
        "reconciled outcomes",
        "unconfirmed history retained",
        "superseded",
        "request failure",
        "stale chat response",
        "saved checked receipt after reload",
        "receipt identity guards",
        "active native thread stays unconfirmed",
        "no prompt",
      ],
      evidence,
    }),
  );
} finally {
  await browser?.close();
  await server.close();
}
