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
      id: `build-${turn}`,
      role: "tool",
      toolStatus: "completed",
      text: JSON.stringify({
        type: "commandExecution",
        command: "npm run build",
        aggregatedOutput: "build ok",
        exitCode: 0,
      }),
    },
  ];
  items = seed();
  const transcript = () => ({
    items,
    order: items.map((i) => i.id),
    replace: true,
    agent: { ...lead, status: "running", inFlight: true, turnId: turn },
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
    /Read 2 files · Ran 1 command/,
  );
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
  await finish();
  await page.locator('[data-message="final-first"]').waitFor();
  assert.equal(
    await work().getAttribute("open"),
    null,
    "final answer collapses untouched work while following",
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
  await emit();
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
  items = seed();
  await emit();
  await page.locator('[data-message="note-reader-12"]').waitFor();
  await page.locator("#messages").evaluate((root) => {
    root.scrollTop = 700;
    root.dispatchEvent(new Event("scroll"));
  });
  await page.locator("#jump-latest").waitFor();
  const anchor = page.locator('[data-message="note-reader-4"]');
  const top = (await anchor.boundingBox()).y;
  await finish();
  assert.notEqual(
    await work().getAttribute("open"),
    null,
    "answer does not hide the work someone is reading",
  );
  assert.ok(
    Math.abs((await anchor.boundingBox()).y - top) < 2,
    "answer arrival preserves the reader's paragraph",
  );
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await emit();
  assert.notEqual(
    await work().getAttribute("open"),
    null,
    "chat switch retains automatic expansion for the reader",
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
  await page.setViewportSize({ width: 1100, height: 900 });
  await page.unroute("**/api/transcript?*");
  await page.reload();
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator("#team-toggle .team-button-count").waitFor();
  assert.match(
    await page.locator("#team-toggle .team-button-count").innerText(),
    /^\d+\/40$/,
  );
  await page.locator('[data-action="stop-team"]').waitFor();
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
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
