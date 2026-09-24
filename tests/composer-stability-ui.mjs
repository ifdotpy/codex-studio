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
  { stdio: ["pipe", "pipe", "pipe"] },
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
  const originalTranscript = await (
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
    const transcript = structuredClone(originalTranscript);
    const page = await browser.newPage({
      viewport: { width: 1440, height: 960 },
    });
    page.setDefaultTimeout(12000);
    let queue = [],
      pendingSend,
      pendingStop,
      resolveSendCapture,
      sendCaptured = new Promise((resolve) => {
        resolveSendCapture = resolve;
      });
    const waitForSend = () =>
      Promise.race([
        sendCaptured,
        page.waitForTimeout(12000).then(() => {
          throw Error("send API request was not captured");
        }),
      ]);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/transcript/stream?*", (route) =>
      route.fulfill({
        contentType: "text/event-stream",
        body: `data: ${JSON.stringify({ ...transcript, items: transcript.items || [], order: (transcript.items || []).map((item) => item.id), replace: true })}\n\n`,
      }),
    );
    await page.route("**/api/transcript?*", (route) =>
      route.fulfill({ json: transcript }),
    );
    await page.route("**/api/queue?*", (route) =>
      route.fulfill({ json: { items: queue } }),
    );
    await page.route("**/api/messages", (route) => {
      pendingSend = route;
      resolveSendCapture?.();
      resolveSendCapture = null;
    });
    await page.route("**/api/stop", (route) => {
      pendingStop = route;
    });
    await page.setViewportSize({ width, height: 960 });
    await page.goto(origin);
    if (width <= 760)
      await page.getByRole("button", { name: "Toggle conversations" }).click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page.locator("#composer").waitFor();
    await page.locator("#stop:disabled").waitFor();
    await page.locator("#message").fill("Check this task");
    const boxes = () =>
      page.evaluate((mobile) => {
        const result = {};
        for (const selector of [
          "#composer",
          ...(mobile ? [] : ["#conversation-title"]),
          "#message",
          ".composer-bar",
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
    const waitForStableGeometry = async () => {
      const selectors = Object.keys(baseline);
      await page.evaluate(() => {
        window.composerGeometryFrame = null;
      });
      await page.waitForFunction((selectors) => {
        const current = selectors.map((selector) => {
          const rect = document.querySelector(selector).getBoundingClientRect();
          return [rect.x, rect.y, rect.width, rect.height].map(Math.round);
        });
        const serialized = JSON.stringify(current);
        const stable = window.composerGeometryFrame === serialized;
        window.composerGeometryFrame = serialized;
        return stable;
      }, selectors);
    };
    const stable = async (label) => {
      await waitForStableGeometry();
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
      fixture.stdin.write(
        JSON.stringify({
          method: "fixture/agent-status",
          params: { agent: lead.id, status: next },
        }) + "\n",
      );
      await page.evaluate(() => window.dispatchEvent(new Event("online")));
      await page.waitForFunction(
        (expected) => document.querySelector("#stop").disabled === expected,
        next === "completed",
      );
      await page.waitForTimeout(80);
    };
    const waitForComposerSend = () =>
      page.waitForFunction(() =>
        document.querySelector("#send-state").textContent.includes("Sending"),
      );
    const firstSendStarted = waitForComposerSend();
    await page.locator("#send").click();
    await firstSendStarted;
    await stable("sending");
    await waitForSend();
    await pendingSend.fulfill({
      json: { id: pendingSend.request().postDataJSON().id, status: "sent" },
    });
    await page.waitForFunction(() => !document.querySelector("#message").value);
    await page.waitForFunction(
      () => !document.querySelector("#send-state").textContent,
    );
    await stable("sent and draft cleared");
    for (const next of ["starting", "running", "approval"]) {
      await updatePhase(next);
      await stable(next);
    }
    await page.locator("#message").fill("Additional instruction");
    pendingSend = null;
    sendCaptured = new Promise((resolve) => {
      resolveSendCapture = resolve;
    });
    await updatePhase("completed");
    await page.locator("#send").click();
    await waitForSend();
    await stable("sending before failed delivery");
    assert.equal(pendingSend.request().postDataJSON().delivery, "after_tool");
    await pendingSend.fulfill({
      status: 400,
      json: { error: "Fixture delivery failed" },
    });
    await page.getByText("Fixture delivery failed", { exact: true }).waitFor();
    await stable("failed delivery is recoverable");
    await page
      .getByRole("button", { name: "Restore draft", exact: true })
      .click();
    await stable("failed message restored to draft");
    assert.equal(
      await page.locator("#message").inputValue(),
      "Additional instruction",
    );
    await updatePhase("running");
    queue = [
      {
        id: "queued-fixture",
        text: "Later instruction",
        delivery: "queue",
        status: "queued",
      },
    ];
    await page.reload();
    if (width <= 760)
      await page.getByRole("button", { name: "Toggle conversations" }).click();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page
      .getByRole("button", { name: "Delete queued message 1", exact: true })
      .waitFor();
    assert.equal(await page.locator(".message-queue").count(), 1);
    await page
      .locator(".message-queue")
      .getByText("Later instruction", { exact: true })
      .waitFor();
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
