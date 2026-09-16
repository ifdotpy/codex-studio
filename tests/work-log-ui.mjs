#!/usr/bin/env node
// Final answers, work disclosure, manual choice, and scroll intent. No model calls.
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
const directory = await mkdtemp(join(tmpdir(), "studio-work-log-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), directory],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (d) => {
  log += d;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((a) => a.name === "Other project");
  let turn = "first",
    turnDone = false,
    phase = "tool",
    items = [];
  const shared = () => ({ turnId: turn });
  const seed = () => [
    {
      ...shared(),
      id: `ask-${turn}`,
      role: "user",
      text: "Inspect the inputs and run the build.",
    },
    ...Array.from({ length: 16 }, (_, i) => ({
      ...shared(),
      id: `note-${turn}-${i}`,
      role: "assistant",
      text: `Progress ${i}.\n\n${"A paragraph with the observed result. ".repeat(8)}`,
    })),
    ...Array.from({ length: 3 }, (_, i) => ({
      ...shared(),
      id: `inspect-${turn}-${i}`,
      role: "tool",
      toolStatus: "completed",
      text: JSON.stringify({
        type: "dynamicToolCall",
        tool: "inspect_artifact",
        status: "completed",
        success: true,
        output: `Artifact ${i} is valid`,
      }),
    })),
    {
      ...shared(),
      id: `read-${turn}`,
      role: "tool",
      toolStatus: "completed",
      text: JSON.stringify({
        type: "commandExecution",
        command: "cat a.ts b.ts",
        commandActions: [
          { type: "read", path: "a.ts" },
          { type: "read", path: "b.ts" },
        ],
        exitCode: 0,
      }),
    },
    {
      ...shared(),
      id: `check-${turn}`,
      role: "tool",
      toolStatus: "completed",
      text: JSON.stringify({
        type: "commandExecution",
        command: "npm run check",
        aggregatedOutput: "checks ok",
        exitCode: 0,
      }),
    },
    {
      ...shared(),
      id: `build-${turn}`,
      role: "tool",
      toolStatus: "running",
      text: JSON.stringify({
        type: "commandExecution",
        command: "npm run build",
        status: "inProgress",
      }),
    },
  ];
  items = seed();
  const transcript = () => ({
    items,
    order: items.map((i) => i.id),
    replace: true,
    agent: {
      ...lead,
      status: turnDone ? "idle" : "running",
      inFlight: !turnDone,
      turnId: turnDone ? null : turn,
      activity: { phase },
    },
  });
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.addInitScript(() => {
    window.streams = [];
    window.EventSource = class extends EventTarget {
      constructor(url) {
        super();
        this.url = url;
        window.streams.push(this);
      }
      close() {
        window.streams = window.streams.filter((s) => s !== this);
      }
    };
  });
  await page.route("**/api/sync/**", (r) =>
    r.fulfill({ status: 404, json: { error: "HTTP fixture" } }),
  );
  await page.route("**/api/transcript?*", (r) =>
    new URL(r.request().url()).searchParams.get("id") === lead.id
      ? r.fulfill({ json: transcript() })
      : r.fallback(),
  );
  const emit = async () => {
    await page.evaluate(
      ({ id, data }) => {
        for (const s of window.streams)
          if (new URL(s.url, location.href).searchParams.get("id") === id)
            s.onmessage?.({ data: JSON.stringify(data) });
      },
      { id: lead.id, data: transcript() },
    );
    await page.waitForTimeout(100);
  };
  const finish = async () => {
    items.push({
      ...shared(),
      id: `final-${turn}`,
      role: "assistant",
      phase: "final_answer",
      streaming: true,
      text: "The build passed.\n\nBoth input files are valid.",
    });
    await emit();
  };
  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  for (const name of ["Plan", "Rules"]) {
    assert.equal(
      await page
        .locator(".workspace-shortcuts")
        .getByRole("button", { name, exact: true })
        .count(),
      0,
    );
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.getByRole("menuitem", { name, exact: true }).click();
    const drawer = page.locator(".workspace-drawer");
    await drawer.getByRole("heading", { name, exact: true }).waitFor();
    await page.keyboard.press("Escape");
    await drawer.waitFor({ state: "hidden" });
  }
  const work = () => page.locator(`[data-turn="${turn}"] .turn-work`);
  await work().waitFor();
  assert.match(
    await work().locator(":scope > summary").innerText(),
    /3 tool calls/,
  );
  assert.equal(
    await page
      .locator(
        `[data-message="read-${turn}"], [data-message="check-${turn}"], [data-message="build-${turn}"]`,
      )
      .count(),
    3,
    "current turn includes completed and running commands",
  );
  assert.equal(await work().evaluate((element) => element.open), true);
  assert.equal(
    await work().locator(".tool-card:visible").count(),
    6,
    "all current calls are visible without expanding the group",
  );
  const build = page.locator(`[data-message="build-${turn}"]`);
  await build.evaluate((element) => {
    element.dataset.retained = "yes";
  });
  items = items.map((item) =>
    item.id === `build-${turn}`
      ? {
          ...item,
          toolStatus: "failed",
          text: JSON.stringify({
            type: "commandExecution",
            command: "npm run build",
            status: "completed",
            exitCode: 1,
            aggregatedOutput: "fixture build failed",
          }),
        }
      : item,
  );
  phase = "thinking";
  items.push({
    ...shared(),
    id: "reasoning-live",
    role: "reasoning",
    text: "",
    reasoningMs: 0,
    reasoningSince: Date.now() / 1000,
    reasoningObservedAt: Date.now() / 1000,
  });
  await emit();
  await page.locator('.reasoning-duration[data-running="true"]').waitFor();
  assert.equal(
    await build.isVisible(),
    true,
    "completed call remains visible during thinking",
  );
  assert.equal(
    await build.getAttribute("data-retained"),
    "yes",
    "call keeps its DOM node",
  );
  assert.equal(await build.getAttribute("data-tool-status"), "failed");
  assert.equal(
    await work().locator(".tool-group").count(),
    0,
    "one disclosure without nested tool-group summaries",
  );
  assert.equal(
    await work().locator(".tool-card[open]").count(),
    0,
    "raw tool content stays closed",
  );
  await build.locator(":scope > summary").click();
  await build.getByText("Exit code 1", { exact: true }).waitFor();
  await build.getByText("fixture build failed", { exact: true }).waitFor();
  if (await page.locator("#jump-latest").isVisible())
    await page.locator("#jump-latest").click();
  await page.screenshot({
    path: join(directory, "thinking-tools-desktop.png"),
  });
  await page.setViewportSize({ width: 390, height: 900 });
  await page.waitForTimeout(100);
  if (await page.locator("#jump-latest").isVisible())
    await page.locator("#jump-latest").click();
  const toolBounds = await build.boundingBox();
  assert.ok(
    toolBounds.y >= 0 && toolBounds.y < 900,
    "call is in the mobile viewport",
  );
  await page.screenshot({ path: join(directory, "thinking-tools-mobile.png") });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  );
  await page.setViewportSize({ width: 1280, height: 900 });
  await work().locator(":scope > summary").click();
  await finish();
  await page.locator('[data-message="final-first"]').waitFor();
  assert.equal(
    await work().getAttribute("open"),
    null,
    "manual collapse remains when the final arrives",
  );
  assert.equal(await page.locator(".turn-answer").isVisible(), true);
  await work().locator(":scope > summary").click();
  await emit();
  assert.notEqual(
    await work().getAttribute("open"),
    null,
    "manual expansion survives streamed updates",
  );
  items = items.map((item) => ({
    ...item,
    streaming: false,
    turnStatus: "completed",
  }));
  turnDone = true;
  await emit();
  assert.equal(
    await build.getAttribute("data-retained"),
    "yes",
    "turn completion preserves visible tool nodes",
  );
  await page.reload();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await work().waitFor();
  assert.notEqual(
    await work().getAttribute("open"),
    null,
    "manual choice survives reload",
  );
  await work().locator(":scope > summary").click();
  assert.equal(
    await page.locator('[data-message="final-first"]').isVisible(),
    true,
    "closing work never hides the answer",
  );
  await page.screenshot({ path: join(directory, "answer-desktop.png") });

  turn = "reader";
  turnDone = false;
  items = seed();
  await emit();
  await page.locator('[data-message="note-reader-12"]').waitFor();
  await work().locator(":scope > summary").click();
  await page.locator("#messages").evaluate((root) => {
    root.scrollTop = 700;
    root.dispatchEvent(new Event("scroll"));
  });
  await page.locator("#jump-latest").waitFor();
  const anchor = page.locator('[data-message="note-reader-4"]');
  const top = (await anchor.boundingBox()).y;
  await finish();
  assert.equal(await work().getAttribute("open"), null);
  assert.equal(
    await anchor.isVisible(),
    true,
    "commentary stays visible outside closed tools",
  );
  assert.ok(
    Math.abs((await anchor.boundingBox()).y - top) < 2,
    "answer arrival preserves the reader's paragraph",
  );
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await emit();
  assert.equal(
    await work().getAttribute("open"),
    null,
    "chat switch retains closed tools",
  );
  assert.ok(
    Math.abs((await anchor.boundingBox()).y - top) < 2,
    "chat switch restores the paragraph beside a final answer",
  );
  await page.locator("#jump-latest").click();
  await work().locator(":scope > summary").click();
  await page.setViewportSize({ width: 390, height: 900 });
  await page.waitForTimeout(250);
  await page.screenshot({ path: join(directory, "answer-mobile.png") });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
    "narrow layout has no horizontal overflow",
  );
  // Reproduce the owner's screenshot: notifications, commentary, then command.
  turn = "interleaved";
  const tool = (id, label) => ({
    ...shared(),
    id,
    role: "output",
    toolStatus: "completed",
    text: JSON.stringify({
      type: "dynamicToolCall",
      tool: label,
      status: "completed",
      success: true,
    }),
  });
  items = [
    tool("worker-result", "Worker result received"),
    tool("agent-message", "Agent message received"),
    {
      ...shared(),
      id: "between-tools",
      role: "assistant",
      phase: "commentary",
      text: "Получены усиленные нативные тесты и исправление проверки seal.\n\nПроверяю их diff и журналы.",
    },
    tool("next-command", "Run command"),
  ];
  await emit();
  const middle = page.locator('[data-message="between-tools"]');
  await middle.waitFor();
  const blocks = page.locator('[data-turn="interleaved"] .turn-work');
  assert.equal(
    await blocks.count(),
    2,
    "commentary splits consecutive tool groups",
  );
  assert.equal(
    await middle.locator("xpath=ancestor::details").count(),
    0,
    "text is never inside a tool disclosure",
  );
  assert.equal(await blocks.locator(".message").count(), 0);
  for (const [index, count] of [2, 1].entries()) {
    const block = blocks.nth(index);
    assert.equal(await block.evaluate((element) => element.tagName), "DETAILS");
    assert.equal(await block.locator(":scope > summary").count(), 1);
    assert.notEqual(
      await block.getAttribute("open"),
      null,
      "small groups start open",
    );
    assert.equal(await block.locator(".tool-card").count(), count);
    for (const card of await block.locator(".tool-card").all())
      assert.equal(await card.isVisible(), true, `${count} tools stay visible`);
    assert.equal(await block.locator(".tool-card[open]").count(), 0);
  }
  assert.equal(await middle.isVisible(), true);
  assert.equal(
    await page.evaluate(() => {
      const text = document.querySelector('[data-message="between-tools"]');
      const blocks = document.querySelectorAll(
        '[data-turn="interleaved"] .turn-work',
      );
      return (
        !!(
          blocks[0].compareDocumentPosition(text) &
          Node.DOCUMENT_POSITION_FOLLOWING
        ) &&
        !!(
          text.compareDocumentPosition(blocks[1]) &
          Node.DOCUMENT_POSITION_FOLLOWING
        )
      );
    }),
    true,
    "timeline order is preserved",
  );
  await page.setViewportSize({ width: 1280, height: 900 });
  await middle.scrollIntoViewIfNeeded();
  await page.screenshot({
    path: join(directory, "commentary-between-tools.png"),
  });
  // New tools preserve the group and the user's open card.
  await blocks
    .first()
    .locator(".tool-card")
    .first()
    .locator(":scope > summary")
    .click();
  await blocks
    .first()
    .locator(".tool-card")
    .first()
    .evaluate((element) => (element.dataset.retained = "yes"));
  items.splice(2, 0, tool("third-result", "Third result received"));
  await emit();
  assert.equal(
    await blocks.first().evaluate((element) => element.tagName),
    "DETAILS",
  );
  assert.notEqual(await blocks.first().getAttribute("open"), null);
  assert.equal(await blocks.first().locator(".tool-card").count(), 3);
  assert.equal(
    await blocks
      .first()
      .locator(".tool-card")
      .first()
      .getAttribute("data-retained"),
    "yes",
  );
  assert.notEqual(
    await blocks.first().locator(".tool-card").first().getAttribute("open"),
    null,
  );
  await blocks.first().locator(":scope > summary").click();
  items.splice(2, 1);
  await emit();
  assert.equal(
    await blocks.first().getAttribute("open"),
    null,
    "the closed choice survives fewer items",
  );
  await page.reload();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await middle.waitFor();
  assert.equal(
    await blocks.first().evaluate((element) => element.tagName),
    "DETAILS",
  );
  assert.equal(await blocks.first().locator(":scope > summary").count(), 1);
  assert.equal(
    await blocks.first().getAttribute("open"),
    null,
    "the user's closed choice survives reload",
  );
  assert.equal(
    await blocks.first().locator(".tool-card").count(),
    0,
    "an unvisited closed group stays lazy",
  );
  await blocks.first().locator(":scope > summary").click();
  assert.equal(await blocks.first().locator(".tool-card").count(), 2);
  assert.equal(
    await blocks.first().locator(".tool-card").first().isVisible(),
    true,
    "the user can reopen the small group",
  );
  assert.equal(await middle.isVisible(), true);
  await page.setViewportSize({ width: 390, height: 900 });
  await page.screenshot({ path: join(directory, "commentary-mobile.png") });
  await page.setViewportSize({ width: 1100, height: 900 });
  await page.unroute("**/api/transcript?*");
  await page.reload();
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator("#team-toggle .team-button-count").waitFor();
  assert.match(
    await page.locator("#team-toggle .team-button-count").innerText(),
    /^\d+\/40$/,
  );
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page.locator('[data-action="stop-team"]').waitFor();
  await page.keyboard.press("Escape");
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  assert.equal(
    await page.locator('[data-action="stop-team"]').count(),
    0,
    "idle team has no stop action",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      evidence: directory,
      cases: [
        "visible final",
        "single structured work log",
        "completed calls remain visible during thinking",
        "running calls and failures preserve their nodes",
        "current groups start open with tool details closed",
        "small groups start open; added tools preserve expansion",
        "small groups preserve saved closed choice",
        "manual choice",
        "reload",
        "reader anchor",
        "mobile",
        "Plan and Rules menu",
        "team counts and idle controls",
      ],
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(directory, "failure.png") });
  console.error("Evidence:", directory);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
