#!/usr/bin/env node
// Large transcript through the production renderer. All runtime state is isolated.
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
const dir = await mkdtemp(join(tmpdir(), "studio-large-history-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let log = "",
  browser,
  page;
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((x) => x.name === "Other project");
  const items = [];
  const turns = Number(process.env.HISTORY_TURNS || 70);
  const steps = Number(process.env.HISTORY_STEPS || 12);
  for (let t = 0; t < turns; t++) {
    const shared = { turnId: `turn-${t}`, turnStatus: "completed" };
    items.push({
      ...shared,
      id: `user-${t}`,
      role: "user",
      text: `Review component ${t}`,
    });
    for (let m = 0; m < steps; m++) {
      items.push({
        ...shared,
        id: `text-${t}-${m}`,
        role: "assistant",
        text: `Checking component ${t}, part ${m}.\n\n${"A paragraph with **evidence** and a short explanation. ".repeat(8)}\n\n- First check\n- Second check\n\n\`\`\`ts\nconst checked = true;\n\`\`\``,
      });
      items.push({
        ...shared,
        id: `tool-${t}-${m}`,
        role: "tool",
        toolStatus: "completed",
        title: "commandExecution",
        text: JSON.stringify({
          type: "commandExecution",
          command: `python3 check_${m}.py`,
          status: "completed",
          aggregatedOutput: "check passed\n".repeat(
            Number(process.env.HISTORY_TOOL_LINES || 60),
          ),
          exitCode: 0,
        }),
      });
    }
    items.push({
      ...shared,
      id: `result-${t}`,
      role: "assistant",
      phase: "final_answer",
      text: `Component ${t} checked. The final result is available.`,
    });
  }
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
  page.setDefaultTimeout(30000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.addInitScript(() => {
    window.EventSource = class extends EventTarget {
      close() {}
    };
    window.longTasks = [];
    new PerformanceObserver((list) =>
      window.longTasks.push(...list.getEntries().map((x) => x.duration)),
    ).observe({ type: "longtask", buffered: true });
  });
  // Small tool groups open by default. Exercise a reader's saved collapsed
  // history here; turn-history-ui separately checks the small-group default.
  await page.addInitScript(
    ({ key, ids }) => {
      localStorage.setItem(
        key,
        JSON.stringify(Object.fromEntries(ids.map((id) => [id, false]))),
      );
    },
    {
      key: `studio-turns:${state.stateDir}:${lead.id}:tools-v3`,
      ids: items.filter((item) => item.role === "tool").map((item) => item.id),
    },
  );
  await page.route("**/api/sync/**", (r) =>
    r.fulfill({
      status: 404,
      json: { error: "Isolated transcript transport" },
    }),
  );
  let deliveredAt;
  await page.route("**/api/transcript?*", async (r) => {
    const selected =
      new URL(r.request().url()).searchParams.get("id") === lead.id;
    if (selected) deliveredAt = Date.now();
    await r.fulfill({
      json: { agent: selected ? lead : null, items: selected ? items : [] },
    });
  });
  await page.goto(origin);
  const start = Date.now();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.locator(`[data-message="result-${turns - 1}"]`).waitFor();
  const painted = Date.now();
  const measurement = {
    records: items.length,
    clickToContent: painted - start,
    responseToContent: painted - deliveredAt,
    elements: await page.locator("#messages *").count(),
    longTasks: await page.evaluate(() => window.longTasks),
  };
  console.log(JSON.stringify(measurement));
  await page.screenshot({ path: join(dir, "large-history.png") });
  if (process.env.HISTORY_BASELINE !== "1") {
    // Commentary is now visible by contract. Only tool bodies are deferred.
    assert.equal(
      await page.locator(".tool-card").count(),
      0,
      "Closed tool groups do not mount tool bodies",
    );
    assert.equal(
      await page.locator('[data-message^="text-"]').count(),
      turns * steps,
      "All commentary remains in the conversation",
    );
    assert.equal(
      await page.locator('.turn-work [data-message^="text-"]').count(),
      0,
      "No commentary is hidden inside tools",
    );
    assert.ok(
      measurement.responseToContent < 1800,
      "Large history becomes readable without a multi-second render",
    );
    await page
      .locator('[data-message="result-0"]')
      .waitFor({ state: "visible" });
    await page
      .locator('[data-turn="turn-0"] .turn-work > summary')
      .first()
      .click();
    await page.locator('[data-message="text-0-0"]').waitFor();

    await page.locator('[data-message="tool-0-0"] > summary').click();
    await page.locator('[data-message="tool-0-0"] .tool-output').waitFor();
    assert.match(
      await page.locator('[data-message="tool-0-0"] .tool-output').innerText(),
      /check passed/,
    );
    const tool = await page.locator('[data-message="tool-0-0"]').elementHandle();
    const output = await page
      .locator('[data-message="tool-0-0"] .tool-output')
      .elementHandle();
    const group = page.locator('[data-turn="turn-0"] .turn-work').first();
    await group.locator(":scope > summary").click();
    assert.equal(await tool.evaluate((node) => node.isConnected), true);
    assert.equal(await output.evaluate((node) => node.isConnected), true);
    await group.locator(":scope > summary").click();
    await page.locator('[data-message="tool-0-0"] .tool-output').waitFor();
    assert.equal(
      await tool.evaluate((node) => node.open),
      true,
      "Reopening the group preserves the selected tool details",
    );
  }
  assert.deepEqual(errors, []);
  console.log("Large history UI: PASS", dir);
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
}
