#!/usr/bin/env node
// Hidden Chrome, temporary state, real runtime notifications and existing SSE.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "studio-token-rate-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser;
proc.stderr.on("data", (data) => (log += data));
const poll = async (fn, label) => {
  for (let i = 0; i < 100; i++) {
    if (await fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw Error(label + " " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = async () =>
    (await (await fetch(origin + "/api/state")).json()).runtime.agents;
  const initial = await state();
  const lead = initial.find((agent) => agent.name === "Release lead");
  const other = initial.find((agent) => agent.name === "Other project");
  const worker = initial.find((agent) => agent.name === "Worker 00");
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1200, height: 900 },
  });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    const Native = window.EventSource;
    window.__rateSources = [];
    window.EventSource = class extends Native {
      constructor(...args) {
        super(...args);
        window.__rateSources.push(this);
      }
    };
  });
  await page.goto(origin);
  const meter = page.locator("#conversation .token-rate");
  const notify = (agent, method, params) =>
    proc.stdin.write(
      JSON.stringify({
        method,
        params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
      }) + "\n",
    );
  const start = async (id) => {
    await page.locator("#message").fill("Exercise token rate");
    await page.locator("#send").click();
    let actor;
    await poll(async () => {
      actor = (await state()).find((agent) => agent.id === id);
      return actor.inFlight && actor.turnId && actor.threadId;
    }, "turn starts");
    return actor;
  };
  await page.locator(`[data-chat="${other.id}"]`).click();
  await meter.waitFor({ state: "attached" });
  assert.equal(await meter.innerText(), "", "no output before the turn");
  const actor = await start(other.id);
  notify(actor, "item/started", {
    item: { id: "rate-answer", type: "agentMessage", text: "" },
  });
  notify(actor, "item/agentMessage/delta", {
    itemId: "rate-answer",
    delta: "x".repeat(160),
  });
  await page.waitForFunction(() =>
    document.querySelector(".token-rate")?.textContent.includes("tok/s"),
  );
  assert.match(await meter.innerText(), /≈.*tok\/s/);
  assert.equal(await meter.getAttribute("data-agent"), other.id);
  notify(actor, "thread/tokenUsage/updated", {
    tokenUsage: {
      total: { totalTokens: 120, outputTokens: 80 },
      last: { totalTokens: 120, outputTokens: 80 },
    },
  });
  await page.waitForFunction(
    () => document.querySelector(".token-rate")?.dataset.estimated === "false",
  );
  assert.doesNotMatch(await meter.innerText(), /≈/);
  notify(actor, "turn/completed", {
    turn: { id: actor.turnId, status: "completed" },
  });
  await page.waitForFunction(
    () => document.querySelector(".token-rate")?.dataset.active === "false",
  );
  await page.waitForTimeout(1200); // Let the final SSE snapshot arrive before controlled animation events.
  const inject = async (rate) =>
    page.evaluate(
      ({ id, turnId, rate }) => {
        const source = window.__rateSources.findLast(
          (source) =>
            source.readyState === 1 &&
            (source.url.includes(encodeURIComponent(`transcript:${id}`)) ||
              source.url.includes(`id=${id}`)),
        );
        if (!source) throw Error("No existing transcript stream");
        source.dispatchEvent(
          new MessageEvent("token-rate", {
            data: JSON.stringify({
              turnId,
              active: false,
              estimated: false,
              rate,
              outputTokens: 100,
            }),
          }),
        );
      },
      { id: other.id, turnId: actor.turnId, rate },
    );
  await page.emulateMedia({ reducedMotion: "reduce" });
  await inject(20);
  await page.waitForFunction(
    () => document.querySelector(".token-rate")?.textContent === "20 tok/s",
  );
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await page.waitForFunction(
    () =>
      document.querySelector(".token-rate")?.dataset.reducedMotion === "false",
  );
  await inject(80);
  await page.waitForTimeout(100);
  const intermediate = Number((await meter.innerText()).split(" ")[0]);
  assert.ok(
    intermediate > 20 && intermediate < 80,
    `animated intermediate ${intermediate}`,
  );
  await page.waitForTimeout(450);
  assert.equal(await meter.innerText(), "80 tok/s");
  await page.emulateMedia({ reducedMotion: "reduce" });
  await inject(140);
  await page.waitForFunction(
    () => document.querySelector(".token-rate")?.textContent === "140 tok/s",
  );
  await page.waitForTimeout(100);
  assert.equal(
    await meter.innerText(),
    "140 tok/s",
    "reduced motion uses the final value",
  );
  assert.equal(await meter.getAttribute("data-reduced-motion"), "true");
  await page.setViewportSize({ width: 390, height: 844 });
  const before = await meter.boundingBox();
  await inject(123456);
  await page.waitForFunction(
    () => document.querySelector(".token-rate")?.dataset.rate === "123456",
  );
  const after = await meter.boundingBox();
  assert.equal(after.width, before.width);
  assert.equal(after.y, before.y);
  assert.equal(after.x, before.x);
  assert.equal(
    await meter.evaluate((node) => getComputedStyle(node).fontVariantNumeric),
    "tabular-nums",
  );
  assert.ok(
    await page
      .locator(".usage-footer")
      .evaluate((node) => node.scrollWidth <= node.clientWidth),
  );
  assert.ok(after.x >= 0 && after.x + after.width <= 390);
  await start(other.id);
  await page.waitForFunction(
    () => document.querySelector(".token-rate")?.textContent === "",
  );
  await page.setViewportSize({ width: 1200, height: 900 });
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.locator("#team-toggle").click();
  const team = page.getByRole("complementary", { name: "Team", exact: true });
  await team.locator(`[data-worker="${worker.id}"]`).click();
  await page.waitForFunction(
    (id) => document.querySelector(".token-rate")?.dataset.agent === id,
    worker.id,
  );
  proc.stdin.write(
    JSON.stringify({
      method: "fixture/agent-status",
      params: {
        agent: worker.id,
        status: "running",
        autoWake: true,
        threadId: "rate-worker-thread",
      },
    }) + "\n",
  );
  let child;
  await poll(async () => {
    child = (await state()).find((agent) => agent.id === worker.id);
    return child.threadId === "rate-worker-thread";
  }, "worker fixture turn");
  notify(child, "turn/started", {
    turn: { id: child.turnId, status: "inProgress" },
  });
  notify(child, "item/started", {
    item: { id: "child-rate", type: "agentMessage", text: "" },
  });
  notify(child, "item/agentMessage/delta", {
    itemId: "child-rate",
    delta: "worker answer ".repeat(20),
  });
  await page.waitForFunction(() =>
    document.querySelector(".token-rate")?.textContent.includes("tok/s"),
  );
  assert.equal(await meter.getAttribute("data-agent"), worker.id);
  assert.equal(await meter.getAttribute("data-active"), "true");
  await page.waitForFunction(
    (id) =>
      document
        .querySelector(`#team .token-rate[data-agent="${id}"]`)
        ?.textContent.includes("tok/s"),
    worker.id,
  );
  assert.equal(
    await team.locator(`.token-rate[data-agent="${worker.id}"]`).count(),
    1,
    "The open worker also has a Team card meter",
  );
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.waitForFunction(
    (id) => document.querySelector(".token-rate")?.dataset.agent === id,
    lead.id,
  );
  assert.equal(
    await meter.innerText(),
    "",
    "the worker rate stays outside the lead chat",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS lead and open worker footer, Team card, estimate correction, reset, tween, reduced motion, 390px stable width",
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  await new Promise((resolve) => proc.once("exit", resolve));
}
