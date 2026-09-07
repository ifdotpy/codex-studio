#!/usr/bin/env node
// Real notification handling, SQLite, HTTP event stream, and browser. No inference.
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
const root = await mkdtemp(join(tmpdir(), "codex-read-activity-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser;
proc.stderr.on("data", (d) => (log += d));
async function poll(fn, label) {
  for (let i = 0; i < 160; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw Error(`${label}: ${log}`);
}
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1200, height: 1000 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("Read files and skills");
  await page.locator("#send").click();
  let agent;
  await poll(async () => {
    const state = await (await fetch(origin + "/api/state")).json();
    agent = state.runtime.agents.find((a) => a.name === "Other project");
    return agent.status === "running" && agent.turnId;
  }, "turn starts");
  const event = (method, item) =>
    proc.stdin.write(
      JSON.stringify({
        method,
        params: { threadId: agent.threadId, turnId: agent.turnId, item },
      }) + "\n",
    );
  const read = (path, name) => ({
    type: "read",
    command: `cat ${path}`,
    name,
    path,
  });
  const file = {
    id: "read-file",
    type: "commandExecution",
    command: "cat src/config.ts",
    cwd: "/workspace",
    status: "inProgress",
    commandActions: [read("/workspace/src/config.ts", "config.ts")],
  };
  event("item/started", file);
  const fileCard = page
    .locator(".tool-card")
    .filter({ has: page.locator(".tool-title", { hasText: "config.ts" }) });
  const group = page.locator(".tool-group").last();
  await group.waitFor();
  assert.equal(
    await group.getAttribute("open"),
    null,
    "running calls start collapsed",
  );
  await group.locator(":scope > summary").click();
  await fileCard.locator(":scope > summary").click();
  await fileCard
    .locator(".tool-title")
    .filter({ hasText: "Read file" })
    .waitFor();
  assert.equal(await fileCard.getAttribute("data-tool-status"), "running");
  await fileCard
    .getByText("/workspace/src/config.ts", { exact: true })
    .waitFor();
  event("item/completed", {
    ...file,
    status: "completed",
    exitCode: 0,
    aggregatedOutput: "export const port = 4620;",
  });
  await poll(
    () =>
      fileCard.getAttribute("data-tool-status").then((s) => s === "completed"),
    "read finishes",
  );
  await fileCard
    .locator(".tool-output")
    .filter({ hasText: "port = 4620" })
    .waitFor();
  const cases = [
    {
      id: "read-skill",
      type: "commandExecution",
      command: "cat skills/review/SKILL.md",
      commandActions: [read("/workspace/skills/review/SKILL.md", "SKILL.md")],
      aggregatedOutput: "Review the complete diff.",
      expected: "Read skill",
      count: 1,
    },
    {
      id: "read-many",
      type: "commandExecution",
      command: "cat one.ts two.ts",
      commandActions: [
        read("/workspace/one.ts", "one.ts"),
        read("/workspace/two.ts", "two.ts"),
      ],
      expected: "Read 2 files",
      count: 2,
    },
    {
      id: "read-fail",
      type: "commandExecution",
      command: "cat missing.md",
      commandActions: [read("/workspace/missing.md", "missing.md")],
      exitCode: 1,
      aggregatedOutput: "No such file",
      expected: "Read file",
      count: 1,
    },
    {
      id: "read-mixed",
      type: "commandExecution",
      command: "cat config.json && npm test",
      commandActions: [
        read("/workspace/config.json", "config.json"),
        { type: "unknown", command: "npm test" },
      ],
      expected: "Run command",
      count: 1,
    },
    {
      id: "nonread",
      type: "commandExecution",
      command: "printf 'Read SKILL.md'",
      commandActions: [{ type: "unknown", command: "printf 'Read SKILL.md'" }],
      expected: "Run command",
      count: 0,
    },
    {
      id: "explicit-file",
      type: "mcpToolCall",
      server: "filesystem",
      tool: "read_file",
      arguments: { path: "/workspace/explicit.txt" },
      result: { content: [{ type: "text", text: "Explicit file output" }] },
      expected: "Read file",
      count: 1,
    },
    {
      id: "explicit-skill",
      type: "dynamicToolCall",
      namespace: "skills",
      tool: "read",
      arguments: { resource: "skill://research/SKILL.md" },
      contentItems: [{ type: "inputText", text: "Explicit skill output" }],
      expected: "Read skill",
      count: 1,
    },
    {
      id: "unsafe-name",
      type: "commandExecution",
      command: "cat special",
      commandActions: [
        read(
          "/workspace/<img src=x onerror=alert(1)>.md",
          "<img src=x onerror=alert(1)>.md",
        ),
      ],
      expected: "Read file",
      count: 1,
    },
  ];
  for (const item of cases) {
    const { expected, count, ...payload } = item;
    event("item/completed", { status: "completed", exitCode: 0, ...payload });
    await poll(
      () =>
        page
          .locator(".tool-card")
          .count()
          .then((n) => n === cases.indexOf(item) + 2),
      item.id,
    );
    const card = page.locator(".tool-card").nth(cases.indexOf(item) + 1);
    assert.equal(
      (await card.locator(".tool-title > span").textContent()).trim(),
      expected,
    );
    assert.equal(
      Number((await card.getAttribute("data-read-count")) || 0),
      count,
    );
    await card.locator(":scope > summary").click();
    await card.locator(".tool-body").waitFor();
    assert.equal(await card.locator(".tool-read-targets li").count(), count);
    if (item.id === "read-fail") {
      assert.equal(await card.getAttribute("data-tool-status"), "failed");
      await card.getByText("Exit code 1").waitFor();
      await card
        .locator(".tool-output")
        .filter({ hasText: "No such file" })
        .waitFor();
    }
    if (item.id === "explicit-file")
      await card
        .locator(".tool-output")
        .filter({ hasText: "Explicit file output" })
        .waitFor();
    if (item.id === "explicit-skill")
      await card
        .locator(".tool-output")
        .filter({ hasText: "Explicit skill output" })
        .waitFor();
  }
  assert.equal(
    await page.locator(".tool-read-targets img").count(),
    0,
    "paths remain text",
  );
  // Closing the group must survive new calls, output, and failures.
  await group.locator(":scope > summary").click();
  event("item/started", {
    id: "compact-live",
    type: "commandExecution",
    command: "npm test",
    status: "inProgress",
  });
  await poll(
    () => group.getAttribute("data-running").then((value) => value === "1"),
    "group reports live work",
  );
  assert.equal(await group.getAttribute("open"), null);
  assert.ok(
    (await group.boundingBox()).height <= 30,
    "collapsed group stays one line",
  );
  event("item/completed", {
    id: "compact-live",
    type: "commandExecution",
    command: "npm test",
    status: "completed",
    exitCode: 2,
    aggregatedOutput: "failed\n".repeat(100),
  });
  await poll(
    () => group.getAttribute("data-failed").then((value) => value === "2"),
    "failures stay visible in the summary",
  );
  assert.equal(await group.getAttribute("open"), null);
  await page.screenshot({ path: join(root, "compact-group.png") });
  event("item/completed", {
    id: "prose-only",
    type: "agentMessage",
    text: "I read another-file.ts and SKILL.md.",
  });
  await page
    .getByText("I read another-file.ts and SKILL.md.", { exact: true })
    .waitFor();
  assert.equal(
    await page.locator(".tool-card").count(),
    10,
    "assistant prose does not invent calls",
  );
  await page.locator(".tool-group > summary").first().scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(root, "read-activity-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator(".tool-group").first().scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(root, "read-activity-mobile.png") });
  assert.equal(
    await page.evaluate(() => document.body.scrollWidth),
    390,
    "no horizontal overflow",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      cases:
        "file, skill, multiple reads, failure, mixed command, nonread, explicit file/skill tools, escaped paths, prose, mobile",
      root,
    }),
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
