#!/usr/bin/env node
// Real batch SSE, hidden Chrome, temp state. No model calls.
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
let browser;
try {
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  for (const [size, width] of [
    [3, 390],
    [8, 1200],
  ]) {
    const root = await mkdtemp(join(tmpdir(), "studio-team-token-rate-"));
    const proc = spawn(
      "python3",
      ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
      {
        stdio: ["pipe", "pipe", "pipe"],
        env: { ...process.env, TOKEN_RATE_WORKER_COUNT: String(size) },
      },
    );
    let log = "",
      timer;
    proc.stderr.on("data", (data) => (log += data));
    const page = await browser.newPage({
      viewport: { width: 1200, height: 900 },
    });
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const poll = async (fn, label) => {
      for (let i = 0; i < 150; i++) {
        if (await fn()) return;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      throw Error(label + " " + log);
    };
    try {
      const port = await new Promise((resolve, reject) => {
        proc.stdout.once("data", (data) =>
          resolve(Number(String(data).trim())),
        );
        proc.once("exit", () => reject(Error(log)));
      });
      const origin = `http://127.0.0.1:${port}`;
      const state = async () =>
        (await (await fetch(origin + "/api/state")).json()).runtime.agents;
      const agents = await state();
      const lead = agents.find((agent) => agent.name === "Release lead");
      const workers = agents
        .filter((agent) => agent.rootId === lead.id && agent.id !== lead.id)
        .sort((a, b) => a.name.localeCompare(b.name));
      const running = workers.slice(0, size === 3 ? 2 : 3);
      const idle = workers[running.length];
      await page.addInitScript(() => {
        const Native = window.EventSource;
        window.__teamRateSources = [];
        window.__teamRateBatches = [];
        window.EventSource = class extends Native {
          constructor(...args) {
            super(...args);
            if (this.url.includes("/api/token-rates/stream")) {
              window.__teamRateSources.push(this);
              this.addEventListener("token-rates", (event) => {
                const batch = JSON.parse(event.data);
                if (!batch.test)
                  window.__teamRateBatches.push({
                    at: performance.now(),
                    ...batch,
                  });
              });
            }
          }
        };
      });
      await page.goto(origin);
      await page.locator(`[data-chat="${lead.id}"]`).click();
      await page.setViewportSize({ width, height: 900 });
      await page.locator("#team-toggle").click();
      const team = page.locator("#team");
      await team.waitFor();
      // Let the mobile drawer finish its entrance before geometry checks.
      await page.waitForTimeout(350);
      assert.equal(
        await team.getAttribute("class"),
        size === 3 ? "team-compact" : null,
      );
      const meter = (id) => team.locator(`.token-rate[data-agent="${id}"]`);
      const card = (id) =>
        team
          .locator(".worker-entry")
          .filter({ has: page.locator(`[data-worker="${id}"]`) });
      const fixtureStatus = (agent, status) =>
        proc.stdin.write(
          JSON.stringify({
            method: "fixture/agent-status",
            params: {
              agent: agent.id,
              status,
              autoWake: status === "running",
              threadId: "rate-thread-" + agent.id,
            },
          }) + "\n",
        );
      for (const agent of running) fixtureStatus(agent, "running");
      fixtureStatus(idle, "waiting");
      let ready;
      await poll(async () => {
        ready = await state();
        return [...running, idle].every(
          (agent) =>
            ready.find((value) => value.id === agent.id)?.threadId ===
            "rate-thread-" + agent.id,
        );
      }, "fixture threads");
      const notify = (agent, method, params) =>
        proc.stdin.write(
          JSON.stringify({
            method,
            params: {
              threadId: "rate-thread-" + agent.id,
              turnId: "fixture-turn",
              ...params,
            },
          }) + "\n",
        );
      for (const agent of running) {
        notify(agent, "turn/started", {
          turn: { id: "fixture-turn", status: "inProgress" },
        });
        notify(agent, "item/started", {
          item: { id: "answer", type: "agentMessage", text: "" },
        });
      }
      await page.waitForFunction(
        (id) =>
          document
            .querySelector(
              `.token-rate[data-agent="${id}"][data-variant="worker"]`,
            )
            ?.parentElement.querySelector("small")
            ?.textContent.includes("Turn ended"),
        idle.id,
      );
      const before = await card(running[0].id).boundingBox();
      const beforeMeter = await meter(running[0].id).boundingBox();
      assert.equal(await meter(running[0].id).innerText(), "");
      const feed = () =>
        running.forEach((agent, index) =>
          notify(agent, "item/agentMessage/delta", {
            itemId: "answer",
            delta: "x".repeat((index + 1) ** 2 * 80),
          }),
        );
      feed();
      timer = setInterval(feed, 700);
      for (const agent of running)
        await page.waitForFunction(
          (id) =>
            document
              .querySelector(
                `.token-rate[data-agent="${id}"][data-variant="worker"]`,
              )
              ?.textContent.includes("tok/s"),
          agent.id,
        );
      const after = await card(running[0].id).boundingBox();
      const afterMeter = await meter(running[0].id).boundingBox();
      assert.deepEqual(
        [after.width, after.height, after.y],
        [before.width, before.height, before.y],
        "card size and position stay fixed",
      );
      assert.deepEqual(
        [afterMeter.width, afterMeter.x, afterMeter.y],
        [beforeMeter.width, beforeMeter.x, beforeMeter.y],
        "rate slot stays fixed",
      );
      assert.equal(
        await meter(idle.id).innerText(),
        "",
        "idle worker has no rate",
      );
      await page.waitForFunction(
        (ids) =>
          window.__teamRateBatches.filter((batch) =>
            ids.every((id) => batch.rates[id]),
          ).length >= 4,
        running.map((agent) => agent.id),
      );
      const batches = await page.evaluate(
        (ids) =>
          window.__teamRateBatches.filter((batch) =>
            ids.every((id) => batch.rates[id]),
          ),
        running.map((agent) => agent.id),
      );
      for (let i = 1; i < batches.length; i++)
        assert.ok(
          batches[i].at - batches[i - 1].at >= 850,
          "at most one batch per second",
        );
      assert.equal(
        new Set(Object.values(batches.at(-1).rates).map((rate) => rate.rate))
          .size,
        running.length,
        "each worker has its own value",
      );
      assert.equal(
        Object.keys(batches.at(-1).rates).length,
        running.length,
        "one batch contains all active workers and excludes idle workers",
      );
      assert.equal(
        await page.evaluate(() => window.__teamRateSources.length),
        1,
        "one team connection for all cards",
      );
      assert.ok(
        await meter(running[0].id).evaluate(
          (node) =>
            getComputedStyle(node).fontVariantNumeric === "tabular-nums",
        ),
      );
      assert.ok(
        await team.evaluate((node) => node.scrollWidth <= node.clientWidth),
      );
      const injectOnBatch = async (rate) =>
        page.evaluate(
          async ({ teamId, ids, rate }) => {
            const source = window.__teamRateSources.at(-1);
            await new Promise((resolve) =>
              source.addEventListener(
                "token-rates",
                () => {
                  source.dispatchEvent(
                    new MessageEvent("token-rates", {
                      data: JSON.stringify({
                        teamId,
                        test: true,
                        rates: Object.fromEntries(
                          ids.map((id, index) => [
                            id,
                            {
                              turnId: "fixture-turn",
                              active: true,
                              estimated: false,
                              rate: rate * (index + 1),
                              outputTokens: 1000,
                            },
                          ]),
                        ),
                      }),
                    }),
                  );
                  resolve();
                },
                { once: true },
              ),
            );
          },
          { teamId: lead.id, ids: running.map((agent) => agent.id), rate },
        );
      await page.emulateMedia({ reducedMotion: "reduce" });
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.dataset.reducedMotion === "true",
        running[0].id,
      );
      await injectOnBatch(20);
      assert.equal(await meter(running[0].id).innerText(), "20 tok/s");
      await page.emulateMedia({ reducedMotion: "no-preference" });
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.dataset.reducedMotion === "false",
        running[0].id,
      );
      await meter(running[0].id).evaluate((node) => {
        window.__rateTweenValues = [];
        window.__rateObserver = new MutationObserver(() =>
          window.__rateTweenValues.push(Number(node.textContent.split(" ")[0])),
        );
        window.__rateObserver.observe(node, {
          childList: true,
          subtree: true,
          characterData: true,
        });
      });
      await injectOnBatch(80);
      await page.waitForTimeout(550);
      assert.ok(
        await page.evaluate(() =>
          window.__rateTweenValues.some((value) => value > 20 && value < 80),
        ),
        "card number uses the shared tween",
      );
      await page.evaluate(() => window.__rateObserver.disconnect());
      await page.emulateMedia({ reducedMotion: "reduce" });
      await injectOnBatch(140);
      assert.equal(
        await meter(running[0].id).innerText(),
        "140 tok/s",
        "reduced motion jumps to the target",
      );
      clearInterval(timer);
      timer = undefined;
      notify(running[0], "turn/completed", {
        turn: { id: "fixture-turn", status: "completed" },
      });
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.textContent === "",
        running[0].id,
      );
      await page.locator("#team-close").click();
      await page.waitForFunction(() =>
        window.__teamRateSources.every((source) => source.readyState === 2),
      );
      assert.deepEqual(errors, []);
      console.log(
        `PASS ${size === 3 ? "compact" : "grouped"} Team cards, ${running.length} active workers, idle hidden, 1 batch/s, 1 connection, tween, reduced motion, stable ${width}px`,
      );
    } catch (error) {
      console.error(
        await page.evaluate(() => ({
          sources: window.__teamRateSources.map((source) => ({
            url: source.url,
            state: source.readyState,
          })),
          batches: window.__teamRateBatches.slice(-2),
          meters: [...document.querySelectorAll("#team .token-rate")].map(
            (node) => ({ text: node.textContent, ...node.dataset }),
          ),
        })),
      );
      throw error;
    } finally {
      clearInterval(timer);
      await page.close();
      proc.kill("SIGTERM");
      if (proc.exitCode === null)
        await new Promise((resolve) => proc.once("exit", resolve));
    }
  }
} finally {
  await browser?.close();
}
