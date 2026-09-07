#!/usr/bin/env node
// Production UI with isolated files and recorded transcript fixtures. No model calls.
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
const directory = await mkdtemp(join(tmpdir(), "studio-turn-history-"));
const report = join(
  directory,
  "official-build-report-with-a-long-descriptive-name.md",
);
const picture = join(directory, "build.png");
await writeFile(report, "Verified historical file preview.\nSecond line.");
await writeFile(
  picture,
  Buffer.from(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aL1kAAAAASUVORK5CYII=",
    "base64",
  ),
);
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), directory],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  page,
  log = "";
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const url = `http://127.0.0.1:${port}`;
  const state = await (await fetch(url + "/api/state")).json();
  const lead = state.runtime.agents.find((a) => a.name === "Release lead");
  const initial = await (
    await fetch(url + "/api/transcript?id=" + lead.id)
  ).json();
  const item = (id, role, text, extra = {}) => ({ id, role, text, ...extra });
  const items = [
    item("ask1", "user", "Check the build", {
      turnId: "one",
      turnStatus: "completed",
    }),
    item("note1", "assistant", "First I inspect the build inputs.", {
      turnId: "one",
      turnStatus: "completed",
    }),
    item(
      "patch1",
      "output",
      JSON.stringify({
        type: "fileChange",
        status: "completed",
        changes: [
          {
            path: "src/build.ts",
            diff: "@@ -1 +1 @@\n-old\n+historical build result",
          },
        ],
      }),
      { turnId: "one", turnStatus: "completed", toolStatus: "completed" },
    ),
    item(
      "result1",
      "assistant",
      `Build checks passed.\n\n[Official build report](<${report}>)\n\n![Build image](<${picture}>)\n\n\`\`\`html\n<div>Build preview ready</div>\n\`\`\`\n\n\`\`\`mermaid\nflowchart LR\n A[Build] --> B[Checked]\n\`\`\``,
      { turnId: "one", turnStatus: "completed", phase: "final_answer" },
    ),
    item("ask2", "user", "Run acceptance", {
      turnId: "two",
      turnStatus: "failed",
    }),
    item(
      "result2",
      "assistant",
      "Acceptance failed. The executable exited with code 1.",
      { turnId: "two", turnStatus: "failed", phase: "final_answer" },
    ),
    item("ask3", "user", "Investigate the failure", { turnId: "three" }),
    item("live3", "assistant", "Inspecting the error.\n\nMore", {
      turnId: "three",
      streaming: true,
    }),
    item(
      "tool3",
      "output",
      JSON.stringify({
        type: "commandExecution",
        command: "inspect failure",
        status: "inProgress",
      }),
      { turnId: "three", toolStatus: "running" },
    ),
  ];
  const transcript = {
    ...initial,
    items,
    order: items.map((i) => i.id),
    replace: true,
    agent: {
      ...initial.agent,
      status: "running",
      inFlight: true,
      activity: { phase: "tool" },
    },
  };
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.setDefaultTimeout(12000);
  let fileReads = 0;
  page.on("request", (r) => {
    if (new URL(r.url()).pathname === "/api/file") fileReads++;
  });
  await page.route("**/api/transcript**", (route) => {
    if (new URL(route.request().url()).searchParams.get("id") !== lead.id)
      return route.continue();
    return route.request().url().includes("/stream?")
      ? route.fulfill({
          contentType: "text/event-stream",
          body: "data: " + JSON.stringify(transcript) + "\n\n",
        })
      : route.fulfill({ json: transcript });
  });
  // This fixture supplies transcript records through the HTTP stream fallback.
  await page.route("**/api/sync/identity", (r) =>
    r.fulfill({ status: 404, json: { error: "Unsupported sync" } }),
  );
  await page.goto(url);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const first = page.locator('[data-turn="one"]');
  await first.waitFor();
  assert.equal(
    await page.locator('[data-turn="three"]').count(),
    1,
    "active turn has one work log",
  );
  assert.equal(await first.locator(".turn-work").getAttribute("open"), null);
  assert.match(await first.innerText(), /Build checks passed/);
  assert.equal(await page.locator('[data-message="note1"]').isVisible(), true);
  assert.match(
    await page.locator('[data-turn="two"]').innerText(),
    /Turn failed/,
  );
  await first
    .locator('.turn-answer [data-message="result1"]')
    .waitFor({ state: "visible" });
  const readsBeforeClick = fileReads;
  const more = first.getByRole("button", { name: /Show .* more/ });
  if (await more.count()) await more.click();
  await first
    .getByRole("button", { name: "Official build report", exact: true })
    .click();
  await page
    .getByRole("dialog")
    .getByText(/Verified historical file preview/)
    .waitFor();
  assert.equal(
    fileReads,
    readsBeforeClick + 1,
    "file preview loads on selection",
  );
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  await first.getByRole("button", { name: /Build image/ }).click();
  await page.getByRole("dialog").locator("img").waitFor();
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  await first.getByRole("button", { name: /Patch/ }).click();
  await page
    .getByRole("dialog")
    .getByText(/historical build result/)
    .waitFor();
  await page
    .getByRole("dialog")
    .getByRole("button", { name: /Show message/ })
    .click();
  await page.locator('[data-message="patch1"]').waitFor({ state: "visible" });
  assert.equal(await first.locator(".turn-work").getAttribute("open"), "");
  await first.getByRole("button", { name: /HTML preview/ }).click();
  await page.getByRole("dialog").locator("iframe").waitFor();
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  await first.getByRole("button", { name: /Mermaid diagram/ }).click();
  await page.getByRole("dialog").locator(".rich-preview-diagram").waitFor();
  await page.getByRole("dialog").getByRole("button", { name: "Close" }).click();
  await first.locator(".turn-work > summary").click();
  await page.reload();
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await first.waitFor();
  assert.equal(
    await first.locator(".turn-work").getAttribute("open"),
    null,
    "collapse survives reload",
  );
  await page.locator("#messages").evaluate((el) => (el.scrollTop = 0));
  await page.screenshot({ path: join(directory, "turn-history-desktop.png") });
  await page.setViewportSize({ width: 390, height: 900 });
  await page.screenshot({ path: join(directory, "turn-history-390.png") });
  assert(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
    "no viewport overflow",
  );
  await page.setViewportSize({ width: 1440, height: 1000 });
  await first
    .getByRole("button", { name: "Official build report", exact: true })
    .click();
  await page.getByRole("dialog").waitFor();
  await page.keyboard.press("Escape");
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  assert.equal(await page.locator("[data-turn]").count(), 0);
  assert.equal(await page.getByRole("dialog").count(), 0);
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      passed: true,
      terminalOutcomes: true,
      activeStreaming: true,
      results: true,
      recordedPatches: true,
      persistence: true,
      scope: true,
      evidence: directory,
    }),
  );
} catch (error) {
  console.error(
    "Evidence:",
    directory,
    await page
      ?.getByRole("dialog")
      .innerText()
      .catch(() => ""),
  );
  await page?.screenshot({ path: join(directory, "failure.png") });
  throw error;
} finally {
  if (page) await page.unrouteAll({ behavior: "wait" });
  if (browser) await browser.close();
  proc.kill("SIGTERM");
}
