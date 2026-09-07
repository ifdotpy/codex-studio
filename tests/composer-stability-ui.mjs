#!/usr/bin/env node
// Real production React, isolated HTTP fixture, controlled status and delivery timing.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-composer-stability-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  log = "";
fixture.stderr.on("data", (data) => (log += data));
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(`${origin}/api/state`)).json();
  const lead = initial.threads.find((agent) => agent.name === "Other project");
  const transcript = await (
    await fetch(`${origin}/api/transcript?id=${lead.id}`)
  ).json();
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const measurements = [];
  for (const width of [1440, 900, 390]) {
    const page = await browser.newPage({
      viewport: { width: 1440, height: 960 },
    });
    page.setDefaultTimeout(12000);
    let phase = "completed",
      queue = [],
      pendingSend,
      pendingStop;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const agentState = () => ({
      ...lead,
      status: phase,
      inFlight: ["running", "starting", "approval"].includes(phase),
      turnId: phase === "completed" ? null : "fixture-turn",
    });
    // This fixture tests the legacy API used by servers before mobile sync.
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ status: 404, json: { error: "Not found" } }),
    );
    await page.route("**/api/state", (route) =>
      route.fulfill({
        json: {
          ...initial,
          threads: initial.threads.map((agent) =>
            agent.id === lead.id ? agentState() : agent,
          ),
          runtime: { ...initial.runtime, requests: [] },
        },
      }),
    );
    await page.route("**/api/transcript/stream?*", (route) =>
      route.fulfill({
        contentType: "text/event-stream",
        body: `data: ${JSON.stringify({ ...transcript, agent: agentState(), items: transcript.items || [], order: (transcript.items || []).map((item) => item.id), replace: true })}\n\n`,
      }),
    );
    await page.route("**/api/transcript?*", (route) =>
      route.fulfill({ json: { ...transcript, agent: agentState() } }),
    );
    await page.route("**/api/queue?*", (route) =>
      route.fulfill({ json: { items: queue } }),
    );
    await page.route("**/api/messages", (route) => {
      pendingSend = route;
    });
    await page.route("**/api/stop", (route) => {
      pendingStop = route;
    });
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page.setViewportSize({ width, height: 960 });
    await page.locator("#composer").waitFor();
    await page.locator("#stop:disabled").waitFor();
    await page.locator("#message").fill("Check this task");
    const boxes = () =>
      page.evaluate((mobile) => {
        const result = {};
        for (const selector of [
          "#composer",
          "#message",
          ".composer-bar",
          ".message-delivery",
          ".attach-button",
          ...(mobile ? [] : [".dictation-trigger"]),
          "#stop",
          "#send",
          ...(mobile ? [] : [".usage-footer"]),
        ]) {
          const node = document.querySelector(selector);
          const box = node.getBoundingClientRect();
          result[selector] = Object.fromEntries(
            ["x", "y", "width", "height"].map((key) => [key, box[key]]),
          );
        }
        return result;
      }, width <= 760);
    const baseline = await boxes();
    const stable = async (label) => {
      const current = await boxes();
      measurements.push({ width, label, boxes: current });
      for (const [selector, box] of Object.entries(baseline))
        for (const key of ["x", "y", "width", "height"])
          assert.ok(
            Math.abs(current[selector][key] - box[key]) <= 1,
            `${width}px ${label} ${selector}.${key}: ${box[key]} -> ${current[selector][key]}`,
          );
      assert.equal(
        await page.evaluate(
          () => document.documentElement.scrollWidth > innerWidth,
        ),
        false,
        `${width}px no horizontal page overflow`,
      );
      const form = current["#composer"];
      for (const selector of [
        ".message-delivery",
        ".attach-button",
        ...(width <= 760 ? [] : [".dictation-trigger"]),
        "#stop",
        "#send",
      ])
        assert.ok(
          current[selector].x >= form.x &&
            current[selector].x + current[selector].width <=
              form.x + form.width + 1,
          `${selector} stays inside composer`,
        );
    };
    const updatePhase = async (next) => {
      phase = next;
      await page.evaluate(() => window.dispatchEvent(new Event("online")));
      await page.waitForFunction(
        (expected) => document.querySelector("#stop").disabled === expected,
        next === "completed",
      );
      await page.waitForTimeout(80);
    };
    await page
      .getByRole("button", { name: "After tool call", exact: true })
      .click();
    await stable("idle preference selected");
    await page.locator("#send").click();
    await page.waitForFunction(() =>
      document.querySelector("#send-state").textContent.includes("Sending"),
    );
    await stable("sending");
    await pendingSend.fulfill({ json: { status: "sent" } });
    await page.waitForFunction(
      () => !document.querySelector("#send-state").textContent,
    );
    await stable("sent and draft cleared");
    for (const next of ["starting", "running", "approval"]) {
      await updatePhase(next);
      await stable(next);
    }
    await page.locator("#message").fill("Additional instruction");
    await page.getByRole("button", { name: "After turn", exact: true }).click();
    await page.locator("#send").click();
    await page.waitForFunction(() =>
      document.querySelector("#send-state").textContent.includes("Sending"),
    );
    await stable("sending while running");
    await pendingSend.fulfill({
      status: 503,
      json: { error: "Fixture delivery failed" },
    });
    await page.getByText("Fixture delivery failed", { exact: true }).waitFor();
    await stable("failed delivery retains draft");
    assert.equal(
      await page.locator("#message").inputValue(),
      "Additional instruction",
    );
    queue = [
      {
        id: "queued-fixture",
        text: "Later instruction",
        delivery: "queue",
        status: "queued",
      },
    ];
    await page.getByRole("button", { name: /1 queued message/ }).waitFor();
    await stable("queue appears");
    await page.locator("#stop").click();
    await page.waitForTimeout(80);
    assert.ok(pendingStop, "stop reaches API");
    await stable("stopping request");
    await pendingStop.fulfill({ json: {} });
    await updatePhase("completed");
    await stable("turn completed");
    await page.screenshot({
      path: join(evidence, `composer-${width}.png`),
      fullPage: true,
    });
    assert.deepEqual(errors, []);
    await page.close();
  }
  await writeFile(
    join(evidence, "geometry.json"),
    JSON.stringify(measurements, null, 2),
  );
  console.log(
    `PASS: stable composer geometry across idle, sending, failed delivery, starting, running, approval, queue, stop and completion at 1440, 900 and 390px. ${evidence}`,
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
