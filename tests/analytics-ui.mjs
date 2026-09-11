#!/usr/bin/env node
// Isolated HTTP fixture and deterministic analytics responses. No model calls.
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
const root = await mkdtemp(join(tmpdir(), "codex-analytics-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  log = "";
proc.stderr.on("data", (data) => (log += data));
const observed = [],
  errors = [];
const poll = async (fn, label) => {
  for (let i = 0; i < 120; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(label + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    proc.once("exit", () => reject(new Error(log)));
  });
  const now = Math.floor(Date.now() / 1000);
  const metric = (bytes) => ({
    bytes,
    chars: bytes - 4,
    lines: 20,
    imageCount: 0,
    imageBytes: 0,
  });
  const longName =
    "functions.exec_with_" + "extremely_long_tool_name_".repeat(8);
  const report = {
    version: 1,
    generatedAt: now,
    coverage: {
      trackingSince: now - 86400,
      notes: ["Older stored items can be incomplete."],
    },
    summary: {
      agents: 2,
      turns: 3,
      usageSamples: 4,
      provisionalUsageSamples: 2,
      exactResponseSamples: 3,
      legacyUsageSamples: 0,
      toolCalls: 32,
      modelToolCalls: 2,
      modelFailedToolCalls: 0,
      protocolFailedToolCalls: 1,
      modelDurationMs: 4000,
      protocolDurationMs: 62000,
      protocolToolCalls: 30,
      failedToolCalls: 1,
      compactions: 1,
      inputBytes: 200,
      outputBytes: 4096,
      modelInputBytes: 200,
      modelOutputBytes: 4096,
      protocolInputBytes: 900,
      protocolOutputBytes: 999999,
      durationMs: 62000,
      tokens: {
        inputTokens: 120000,
        cachedInputTokens: 80000,
        cacheWriteInputTokens: null,
        outputTokens: 8000,
        reasoningOutputTokens: 3000,
        totalTokens: 128000,
      },
      tokenObservations: {
        inputTokens: 4,
        cachedInputTokens: 3,
        cacheWriteInputTokens: 0,
        outputTokens: 4,
        reasoningOutputTokens: 4,
        totalTokens: 4,
      },
      cacheHitRate: null,
      peakContextTokens: 100000,
      peakContextPercent: 50,
    },
    agents: [],
    tools: [
      {
        name: longName,
        type: "modelToolCall",
        payloadBoundary: "model",
        calls: 2,
        failed: 0,
        inputBytes: 200,
        outputBytes: 4096,
        modelInputBytes: 200,
        modelOutputBytes: 4096,
        durationMs: null,
      },
      {
        name: "exec_command",
        type: "commandExecution",
        payloadBoundary: "protocol",
        calls: 30,
        failed: 1,
        inputBytes: 900,
        outputBytes: 999999,
        durationMs: 62000,
      },
    ],
    timeline: [
      {
        id: "p1",
        at: now - 200,
        agentId: "a",
        agentName: "Lead",
        turnId: "turn-1",
        model: "gpt-6-astra",
        last: { totalTokens: 100000 },
        modelContextWindow: 200000,
        total: { totalTokens: 110000 },
        delta: { totalTokens: 10000 },
        source: "native",
      },
      {
        id: "p2",
        at: now - 100,
        agentId: "a",
        agentName: "Lead",
        turnId: "turn-2",
        model: "gpt-6-astra",
        last: { totalTokens: 20000 },
        modelContextWindow: 200000,
        source: "native",
      },
      {
        id: "p3",
        at: now - 120,
        agentId: "b",
        agentName: "Worker",
        turnId: "turn-b",
        model: "gpt-5.6-luna",
        last: { totalTokens: 30000 },
        modelContextWindow: 200000,
        source: "native",
      },
    ],
    compactions: [
      {
        id: "compaction-1",
        agentId: "a",
        agentName: "Lead",
        at: now - 150,
        turnId: "turn-2",
        source: "native",
      },
    ],
    calls: Array.from({ length: 32 }, (_, i) => ({
      id: `call-${i}`,
      agentId: "a",
      agentName: "Lead",
      threadId: "thread-" + "a".repeat(180),
      turnId: "turn-1",
      model: "gpt-6-astra",
      accountKey: "account-personal",
      type: i === 0 ? "modelToolCall" : "commandExecution",
      name: i === 0 ? longName : "exec_command",
      payloadBoundary: i === 0 ? "model" : "protocol",
      status: i === 1 ? "failed" : "completed",
      startedAt: now - i,
      finishedAt: now - i + 2,
      durationMs: i === 0 ? null : 2000,
      input: i === 0 ? null : metric(300),
      output: i === 0 ? null : metric(50000),
      modelInput: i === 0 ? metric(200) : null,
      modelOutput: i === 0 ? metric(4096) : null,
      source: "native_rollout",
      command: i === 0 ? null : "python3 -c '" + "x".repeat(2000) + "'",
      error: i === 1 ? "Process exited with code 1" : null,
      coverage: { complete: true },
    })),
    items: [
      { type: "reasoning", count: 2, bytes: 1200, chars: 1000 },
      { type: "userMessage", count: 1, bytes: 500, chars: 450 },
    ],
  };
  report.agentTotals = [
    {
      id: "a",
      name: "Lead",
      tokens: report.summary.tokens,
      toolCalls: 32,
      failedToolCalls: 1,
      compactions: 1,
    },
  ];
  report.modelTotals = [
    { model: "gpt-6-astra", tokens: report.summary.tokens, samples: 3 },
  ];
  report.accountTotals = [
    {
      accountKey: "account-personal",
      tokens: report.summary.tokens,
      samples: 3,
    },
  ];
  report.turns = [
    {
      agentId: "a",
      agentName: "Lead",
      turnId: "turn-1",
      startedAt: now - 100,
      finishedAt: now,
      durationMs: 100000,
      firstOutputDelayMs: 2000,
      status: "completed",
    },
  ];
  report.operations = {
    monitors: [
      {
        id: "monitor-check",
        agent: "a",
        status: "completed",
        created: now - 100,
        bytes: 32768,
        exitCode: 0,
      },
    ],
    eventCounts: { "delivered:message": 9 },
    approvalCounts: { approved: 2 },
  };
  report.notifications = [
    { method: "item/completed", hour: now, count: 10, bytes: 5000, agent: "a" },
    { method: "item/completed", hour: now, count: 5, bytes: 3000, agent: "b" },
  ];
  report.rateLimits = [
    {
      accountKey: "account-personal",
      at: now,
      data: { primary: { usedPercent: 30 } },
    },
  ];
  report.history = [{ agentId: "a", status: "complete", offset: 2000 }];
  let mode = "normal",
    deferred = null;
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 980 },
    acceptDownloads: true,
  });
  page.on("pageerror", (e) => errors.push(e.message));
  await page.route("**/api/analytics?*", async (route) => {
    const q = new URL(route.request().url()).searchParams;
    observed.push(Object.fromEntries(q));
    if (mode === "defer") {
      deferred = route;
      return;
    }
    if (mode === "error") {
      await route.fulfill({
        status: 503,
        json: { error: "Analytics storage unavailable" },
      });
      return;
    }
    const out = structuredClone(report),
      offset = Number(q.get("offset") || 0);
    if (mode === "empty") {
      out.summary = { tokens: {}, toolCalls: 0 };
      out.timeline = [];
      out.calls = [];
      out.tools = [];
      out.compactions = [];
      out.items = [];
      out.turns = [];
      out.agentTotals = [];
      out.modelTotals = [];
      out.accountTotals = [];
      out.operations = {};
      out.notifications = [];
      out.rateLimits = [];
    }
    const all = out.calls.filter(
      (c) => !q.get("tool") || c.name === q.get("tool"),
    );
    out.calls = q.get("export") === "1" ? all : all.slice(offset, offset + 30);
    out.pagination = {
      limit: 30,
      offset,
      total: all.length,
      hasMore: offset + 30 < all.length,
    };
    await route.fulfill({ json: out });
  });
  await page.goto(`http://127.0.0.1:${port}`);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  assert.equal(observed.length, 0, "Closed analytics must not fetch");
  await page.getByRole("button", { name: "Chat context", exact: true }).click();
  assert.equal(observed.length, 0, "Simple context does not fetch analytics");
  await page.getByRole("button", { name: "Advanced analytics" }).click();
  const dialog = page.getByRole("dialog", { name: "Context analytics" });
  await dialog.getByText("128,000", { exact: true }).waitFor();
  assert.equal(
    await dialog
      .locator(".analytics-metrics .analytics-metric")
      .filter({ hasText: "Cache write" })
      .innerText(),
    "Cache write\nUnavailable\nOnly when reported. Partial: 0 of 4 reports",
  );
  const cachedMetric = dialog
    .locator(".analytics-metrics .analytics-metric")
    .filter({ has: page.getByText("Cached input", { exact: true }) });
  assert.match(await cachedMetric.innerText(), /80,000/);
  assert.match(await cachedMetric.innerText(), /Partial: 3 of 4 reports/);
  const totalMetric = dialog
    .locator(".analytics-metrics .analytics-metric")
    .filter({ has: page.getByText("Total", { exact: true }) });
  assert.match(await totalMetric.innerText(), /All 4 reports/);
  const cacheRate = dialog
    .locator(".analytics-secondary .analytics-metric")
    .filter({ hasText: "Cache hit rate" });
  assert.match(await cacheRate.innerText(), /Unavailable/);
  assert.ok(
    await dialog
      .getByText("Exact token use per tool is unavailable.", { exact: false })
      .isVisible(),
  );
  assert.ok(
    await dialog
      .getByText("2 provisional notices excluded", { exact: false })
      .isVisible(),
  );
  assert.equal(
    await dialog.locator(".analytics-chart polyline").count(),
    2,
    "Separate line per agent",
  );
  assert.equal(await dialog.locator(".analytics-chart circle").count(), 3);
  assert.equal(await dialog.locator(".analytics-compaction-marker").count(), 1);
  assert.match(
    await dialog.locator(".analytics-chart circle title").first().textContent(),
    /100,000 \/ 200,000/,
  );
  await page.screenshot({ path: join(root, "overview-1280.png") });
  await dialog.getByRole("tab", { name: /Tool calls/ }).click();
  await dialog
    .getByRole("heading", { name: "Model-visible tool results" })
    .waitFor();
  const modelSection = dialog.locator(".analytics-section").filter({
    has: page.getByRole("heading", { name: "Model-visible tool results" }),
  });
  assert.match(await modelSection.innerText(), /4.0 KiB/);
  assert.doesNotMatch(await modelSection.innerText(), /976.6/);
  await dialog.locator(".analytics-call-row").first().click();
  await dialog.getByRole("heading", { name: "Model-visible result" }).waitFor();
  assert.equal(
    await dialog
      .getByRole("heading", { name: "Protocol output", exact: true })
      .count(),
    0,
  );
  await dialog.locator(".analytics-call-detail summary").click();
  assert.match(
    await dialog.locator(".analytics-call-detail pre").textContent(),
    /"modelOutput"/,
  );
  await page.screenshot({ path: join(root, "calls-1280.png") });
  await dialog.getByRole("button", { name: "Next", exact: true }).click();
  await poll(
    () => observed.some((q) => q.offset === "30"),
    "Pagination request",
  );
  await dialog.getByText("31 to 32 of 32", { exact: true }).waitFor();
  assert.equal(await dialog.locator(".analytics-call-row").count(), 2);
  await dialog.getByLabel("Analytics scope").selectOption("team");
  await poll(
    () => observed.at(-1)?.scope === "team" && observed.at(-1)?.offset === "0",
    "Scope resets pagination",
  );
  await dialog.getByLabel("Analytics period").selectOption("86400");
  await poll(() => Number(observed.at(-1)?.from) > now - 86410, "Period query");
  await dialog.getByLabel("Analytics tool").selectOption(longName);
  await dialog.getByText("1 to 1 of 1", { exact: true }).waitFor();
  assert.ok(
    await dialog
      .getByText("Tool filter applies to tool calls.", { exact: false })
      .isVisible(),
  );
  const downloaded = page.waitForEvent("download");
  await dialog.getByRole("button", { name: "Export JSON" }).click();
  const file = await downloaded;
  const path = join(root, "export.json");
  await file.saveAs(path);
  assert.equal(observed.at(-1).export, "1");
  assert.equal(observed.at(-1).tool, longName);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(root, "resize-390.png") });
  await dialog.locator(".analytics-call-row").first().click();
  await dialog.locator(".analytics-call-detail summary").click();
  const noOverflow = () =>
    dialog.evaluate((el) => el.scrollWidth <= el.clientWidth + 1);
  assert.ok(await noOverflow(), "Modal cannot overflow at390");
  assert.ok(
    await dialog
      .locator(".analytics-scroll")
      .evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
    "Panel cannot overflow at390",
  );
  assert.ok(
    await dialog
      .locator(".analytics-call-detail")
      .evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
    "Long IDs and JSON fit",
  );
  await page.screenshot({ path: join(root, "calls-390.png") });
  await dialog.getByRole("tab", { name: "Activity", exact: true }).click();
  await dialog
    .getByRole("heading", { name: "Compactions", exact: true })
    .waitFor();
  await dialog
    .getByRole("heading", { name: "Usage reports", exact: true })
    .waitFor();
  assert.match(
    await dialog
      .locator(".analytics-section")
      .filter({
        has: page.getByRole("heading", { name: "Models", exact: true }),
      })
      .innerText(),
    /gpt-6-astra/,
  );
  assert.match(
    await dialog
      .locator(".analytics-section")
      .filter({
        has: page.getByRole("heading", { name: "Turns", exact: true }),
      })
      .innerText(),
    /First output 2.0 s/,
  );
  const background = dialog.locator(".analytics-section").filter({
    has: page.getByRole("heading", {
      name: "Background activity",
      exact: true,
    }),
  });
  assert.match(await background.innerText(), /delivered:message/);
  assert.match(await background.innerText(), /monitor-check/);
  const protocol = dialog.locator(".analytics-section").filter({
    has: page.getByRole("heading", { name: "Protocol events", exact: true }),
  });
  assert.match(
    await protocol.locator("tbody").innerText(),
    /item\/completed\s+15\s+7.8 KiB/,
  );
  await dialog
    .getByRole("heading", { name: "Account limit history", exact: true })
    .scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(root, "activity-390.png") });
  mode = "error";
  await dialog.getByRole("button", { name: "Refresh analytics" }).click();
  await dialog
    .getByRole("alert")
    .getByText("Analytics storage unavailable")
    .waitFor();
  assert.ok(
    await dialog
      .getByRole("tab", { name: "Activity", exact: true })
      .isVisible(),
    "Refresh failure preserves the report and tab",
  );
  mode = "empty";
  await dialog.getByRole("button", { name: "Refresh analytics" }).click();
  await dialog.getByText("No provider usage reports recorded.").waitFor();
  await dialog.getByRole("tab", { name: "Overview", exact: true }).click();
  await dialog
    .getByText("No context-window measurements in this period.")
    .waitFor();
  await page.screenshot({ path: join(root, "empty-390.png") });
  mode = "defer";
  await dialog.getByRole("button", { name: "Refresh analytics" }).click();
  await poll(() => !!deferred, "Pending request");
  assert.ok(
    await dialog
      .getByText("No context-window measurements in this period.")
      .isVisible(),
    "Refresh keeps the previous report visible",
  );
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  await deferred.fulfill({ json: report });
  assert.deepEqual(errors, []);
  console.log(
    "PASS analytics UI: filtering, provider subsets, payload boundaries, pagination, export, context/compaction chart, errors, empty and stale responses, 390px overflow.",
  );
  console.log(root);
} finally {
  if (browser) await browser.close();
  proc.kill("SIGTERM");
}
