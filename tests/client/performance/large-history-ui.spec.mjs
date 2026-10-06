import { readTestState, test, spawnFixture as spawn } from "../playwright.mjs";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test("Large history @performance", async ({ context }) => {
  test.setTimeout(180_000);
  const testRepo = fileURLToPath(new URL("../../../", import.meta.url));
  // Large transcript through the production renderer. All runtime state is isolated.
  const repo = testRepo;
  const dir = await mkdtemp(join(tmpdir(), "studio-large-history-"));
  const proc = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "",
    page;
  proc.stderr.on("data", (d) => (log += d));
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await readTestState(origin);
  const lead = state.threads.find((x) => x.name === "Other project");
  const other = state.threads.find((x) => x.name === "Release lead");
  let items = [];
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
  page = await context.newPage();
  await page.setViewportSize({ width: 1440, height: 960 });
  page.setDefaultTimeout(30000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.addInitScript(() => {
    window.EventSource = class extends EventTarget {
      close() {}
    };
    window.largeHistoryClickAt = 0;
    document.addEventListener(
      "pointerdown",
      (event) => {
        if (
          event.target instanceof Element &&
          event.target.closest(
            `[data-chat="${CSS.escape(window.largeHistoryChatId)}"]`,
          )
        )
          window.largeHistoryClickAt = Date.now();
      },
      true,
    );
  });
  await page.addInitScript((id) => (window.largeHistoryChatId = id), lead.id);
  await page.addInitScript(({ key, id }) => localStorage.setItem(key, id), {
    key: `codex-desktop-opened:${state.stateDir}`,
    id: other.id,
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
  const deliveries = [];
  let pageRequests = 0;
  const pageRequestUrls = [];
  await page.route("**/api/transcript?*", async (r) => {
    const selected =
      new URL(r.request().url()).searchParams.get("id") === lead.id;
    if (selected) deliveries.push(Date.now());
    await r.fulfill({
      json: { agent: selected ? lead : null, items: selected ? items : [] },
    });
  });
  await page.route("**/api/transcript/page?*", async (r) => {
    pageRequests++;
    pageRequestUrls.push(r.request().url());
    const url = new URL(r.request().url());
    const cursor = url.searchParams.get("before");
    const end = Math.max(
      0,
      items.findIndex((item) => item.id === cursor),
    );
    const start = Math.max(0, end - 120);
    const selected = items.slice(start, end);
    await r.fulfill({
      json: {
        items: selected,
        nextCursor: start > 0 ? items[start - 1].id : null,
        nextAfterCursor:
          end < items.length ? selected.at(-1)?.id || null : null,
      },
    });
  });
  await page.goto(origin);
  await page.locator(`[data-chat="${other.id}"]`).waitFor({ state: "visible" });
  await page.locator("#messages").waitFor({ state: "visible" });
  await page.evaluate(
    () =>
      new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      ),
  );
  const start = Date.now();
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.locator(`[data-message="result-${turns - 1}"]`).waitFor();
  const clickedAt = await page.evaluate(() => window.largeHistoryClickAt);
  const painted = Date.now();
  assert.ok(clickedAt > 0, "The selected chat receives a pointer event");
  const relevantDelivery = deliveries.find((time) => time >= clickedAt);
  const measurement = {
    records: items.length,
    clickToContent: painted - start,
    pointerToContent: painted - clickedAt,
    responseToContent: painted - (relevantDelivery || clickedAt),
    selectedDeliveries: deliveries.length,
    elements: await page.locator("#messages *").count(),
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
    const initialWindow = items.slice(-240);
    assert.equal(
      await page.locator('[data-message^="text-"]').count(),
      initialWindow.filter((item) => item.id.startsWith("text-")).length,
      "The current page stays within its retained window",
    );
    assert.equal(
      await page.locator('.turn-work [data-message^="text-"]').count(),
      0,
      "No commentary is hidden inside tools",
    );
    const loadEarlier = page.locator("#earlier-messages");
    while (await loadEarlier.count()) {
      const response = page
        .waitForResponse((r) => r.url().includes("/api/transcript/page?"))
        .then(
          (value) => ({ value }),
          (error) => ({ error }),
        );
      await page.locator("#messages").evaluate((root) => {
        root.scrollTop = 0;
        root.dispatchEvent(new Event("scroll"));
      });
      await loadEarlier.click({ timeout: 5000 });
      const result = await response;
      if ("error" in result) {
        throw Error(
          `Earlier page request missing: count=${pageRequests}, button=${await loadEarlier.isVisible()}, disabled=${await loadEarlier.isDisabled()}, errors=${JSON.stringify(errors)}, urls=${JSON.stringify(pageRequestUrls)}. ${result.error}`,
        );
      }
    }
    assert.equal(
      pageRequests > 0,
      true,
      "older transcript pages reload on demand",
    );
    await page.locator('[data-message="text-0-0"]').waitFor();
    assert.ok(
      measurement.responseToContent < 1800,
      "Large history becomes readable without a multi-second render",
    );
    // Completed historical commands are hidden. Reopen the newest turn as
    // active to check its saved disclosure and selected tool details.
    items = items.map((item) => {
      if (item.turnId !== `turn-${turns - 1}`) return item;
      const { turnStatus: _turnStatus, ...activeItem } = item;
      return activeItem;
    });
    await page.reload();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page
      .locator(`[data-message="result-${turns - 1}"]`)
      .waitFor({ state: "visible" });
    await page
      .locator(`[data-turn="turn-${turns - 1}"] .turn-work > summary`)
      .first()
      .click();
    await page.locator(`[data-message="text-${turns - 1}-0"]`).waitFor();
    // The matching data-message can be a hidden lazy placeholder until the
    // work log has committed its visited state. Wait for the real tool card.
    const firstToolSummary = page.locator(
      `[data-message="tool-${turns - 1}-0"] > summary`,
    );
    try {
      await firstToolSummary.waitFor({ state: "visible" });
    } catch (error) {
      const state = await page.evaluate(
        ({ id, stateDir, agentId }) => {
          const turn = document.querySelector(`[data-turn="turn-${id}"]`);
          const work = turn?.querySelector(".turn-work");
          const firstTool = document.querySelector(
            `[data-message="tool-${id}-0"]`,
          );
          return {
            turn: turn?.outerHTML.slice(0, 2400),
            workOpen: work?.open,
            workVisited: work?.querySelectorAll("[data-lazy-message]").length,
            firstTool: firstTool?.outerHTML.slice(0, 1200),
            saved: localStorage.getItem(
              `studio-turns:${stateDir}:${agentId}:tools-v3`,
            ),
          };
        },
        { id: turns - 1, stateDir: state.stateDir, agentId: lead.id },
      );
      throw Error(
        `Newest tool did not render: ${JSON.stringify(state)}. ${error}`,
      );
    }
    await page
      .locator(`[data-message="tool-${turns - 1}-0"] > summary`)
      .click();
    await page
      .locator(`[data-message="tool-${turns - 1}-0"] .tool-output`)
      .waitFor();
    assert.match(
      await page
        .locator(`[data-message="tool-${turns - 1}-0"] .tool-output`)
        .innerText(),
      /check passed/,
    );
    const tool = await page
      .locator(`[data-message="tool-${turns - 1}-0"]`)
      .elementHandle();
    const output = await page
      .locator(`[data-message="tool-${turns - 1}-0"] .tool-output`)
      .elementHandle();
    const group = page
      .locator(`[data-turn="turn-${turns - 1}"] .turn-work`)
      .first();
    await group.locator(":scope > summary").click();
    assert.equal(await tool.evaluate((node) => node.isConnected), true);
    assert.equal(await output.evaluate((node) => node.isConnected), true);
    await group.locator(":scope > summary").click();
    await page
      .locator(`[data-message="tool-${turns - 1}-0"] .tool-output`)
      .waitFor();
    assert.equal(
      await tool.evaluate((node) => node.open),
      true,
      "Reopening the group preserves the selected tool details",
    );
  }
  assert.deepEqual(errors, []);
  console.log("Large history UI: PASS", dir);
});
