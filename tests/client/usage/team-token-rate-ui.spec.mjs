import { readTestState, spawnFixture as spawn, test } from "../playwright.mjs";
// Real batch SSE, hidden Chrome, temp state. No model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

async function collectTeamRateCadence(browser) {
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const root = await mkdtemp(join(tmpdir(), "studio-team-rate-cadence-"));
  const proc = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, TOKEN_RATE_WORKER_COUNT: "3" },
    },
  );
  const context = await browser.newContext({
    viewport: { width: 1200, height: 900 },
  });
  const page = await context.newPage();
  let log = "";
  let timer;
  proc.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log || "Fixture exited")));
    });
    const origin = `http://127.0.0.1:${port}`;
    const agents = (await readTestState(origin)).runtime.agents;
    const lead = agents.find((agent) => agent.name === "Release lead");
    const worker = agents.find(
      (agent) => agent.rootId === lead.id && agent.id !== lead.id,
    );
    await page.addInitScript(() => {
      window.__cadenceFrames = [];
      window.__cadenceNextSource = 0;
      const Native = window.EventSource;
      window.EventSource = class extends Native {
        constructor(...args) {
          super(...args);
          if (!this.url.includes("/api/sync/stream?protocol=3")) return;
          const source = window.__cadenceNextSource++;
          let initial = true;
          this.addEventListener("token-rates", (event) => {
            if (!JSON.parse(event.data).test)
              window.__cadenceFrames.push({
                at: performance.now(),
                source,
                initial,
              });
            initial = false;
          });
        }
      };
    });
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page.locator("#team-toggle").click();
    await page.locator("#team").waitFor();
    proc.stdin.write(
      JSON.stringify({
        method: "fixture/agent-status",
        params: {
          agent: worker.id,
          status: "running",
          autoWake: true,
          threadId: "cadence-thread",
        },
      }) + "\n",
    );
    for (let attempt = 0; attempt < 100; attempt++) {
      const refreshed = (await readTestState(origin)).runtime.agents.find(
        (agent) => agent.id === worker.id,
      );
      if (refreshed.threadId === "cadence-thread") break;
      await new Promise((resolve) => setTimeout(resolve, 50));
      if (attempt === 99)
        throw Error("fixture worker did not become ready: " + log);
    }
    const send = (method, params) =>
      proc.stdin.write(
        JSON.stringify({
          method,
          params: {
            threadId: "cadence-thread",
            turnId: "cadence-turn",
            ...params,
          },
        }) + "\n",
      );
    send("turn/started", {
      turn: { id: "cadence-turn", status: "inProgress" },
    });
    send("item/started", {
      item: { id: "cadence-answer", type: "agentMessage", text: "" },
    });
    let delta = 0;
    const feed = () =>
      send("item/agentMessage/delta", {
        itemId: "cadence-answer",
        delta: "x".repeat(40 + (delta++ % 8) * 30),
      });
    feed();
    timer = setInterval(feed, 200);
    await new Promise((resolve) => setTimeout(resolve, 3200));
    clearInterval(timer);
    timer = undefined;
    return await page.evaluate(() => {
      const previous = new Map();
      const intervals = [];
      for (const frame of window.__cadenceFrames) {
        if (frame.initial) continue;
        const before = previous.get(frame.source);
        if (before)
          intervals.push({
            source: frame.source,
            intervalMs: frame.at - before.at,
          });
        previous.set(frame.source, frame);
      }
      return intervals;
    });
  } finally {
    clearInterval(timer);
    await context.close();
    proc.kill("SIGTERM");
    if (proc.exitCode === null && proc.signalCode === null)
      await new Promise((resolve) => proc.once("exit", resolve));
  }
}

test("team token cadence measurement setup receives changed batches", async ({
  browser,
}) => {
  const intervals = await collectTeamRateCadence(browser);
  assert.ok(
    intervals.length > 0,
    "fixture produces changed token-rate batches",
  );
});

test("team token-rate batches remain at least 850ms apart per EventSource", async ({
  browser,
}) => {
  const intervals = await collectTeamRateCadence(browser);
  console.log(
    "Observed token-rate intervals by EventSource:",
    JSON.stringify(intervals),
  );
  test.fail(
    true,
    "Token-rate batches are event-driven since b64c4fdb; see misc-known-failures.md.",
  );
  expect(
    intervals.every(({ intervalMs }) => intervalMs >= 850),
    JSON.stringify(intervals),
  ).toBe(true);
});

test("team token rate ui", async ({ browser }) => {
  test.setTimeout(240_000);
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
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
      timer,
      follower;
    proc.stderr.on("data", (data) => (log += data));
    const context = await browser.newContext({
      viewport: { width: 1200, height: 900 },
    });
    const page = await context.newPage();
    await page.clock.install();
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
      const state = async () => (await readTestState(origin)).runtime.agents;
      const agents = await state();
      const lead = agents.find((agent) => agent.name === "Release lead");
      const workers = agents
        .filter((agent) => agent.rootId === lead.id && agent.id !== lead.id)
        .sort((a, b) => a.name.localeCompare(b.name));
      const running = workers.slice(0, size === 3 ? 2 : 3);
      const idle = workers[running.length];
      await page.addInitScript(() => {
        const Native = window.EventSource;
        window.__allSources = [];
        window.__teamRateSources = [];
        window.__teamRateBatches = [];
        window.__teamRateInjectedRevision = 0;
        window.EventSource = class extends Native {
          constructor(...args) {
            super(...args);
            const sourceId = window.__teamRateSources.length;
            window.__allSources.push(this);
            if (this.url.includes("/api/sync/stream?protocol=3")) {
              window.__teamRateSources.push(this);
              let initialTokenRates = true;
              this.addEventListener("token-rates", (event) => {
                const batch = JSON.parse(event.data);
                if (!batch.test)
                  window.__teamRateBatches.push({
                    ...batch,
                    sourceId,
                    initial: initialTokenRates,
                    rates: batch.teams[window.__rateTeamId] || {},
                    snapshot: JSON.stringify({
                      rates: batch.rates,
                      teams: batch.teams,
                    }),
                  });
                initialTokenRates = false;
              });
            }
          }
        };
      });
      await page.addInitScript((id) => {
        window.__rateTeamId = id;
      }, lead.id);
      await page.goto(origin);
      await page.locator(`[data-chat="${lead.id}"]`).click();
      await page.waitForFunction(() =>
        window.__allSources.some(
          (source) =>
            source.readyState === 1 && source.url.includes("protocol=3"),
        ),
      );
      const connections = (view) =>
        view.evaluate(() =>
          window.__allSources
            .filter((source) => source.readyState !== 2)
            .map((source) => source.url)
            .sort(),
        );
      const assertConnectionCount = async (view, baseline, message) => {
        const current = await connections(view);
        assert.equal(
          current.length,
          baseline.length,
          `${message}: ${JSON.stringify(current)}`,
        );
      };
      const cardsOffConnections = await connections(page);
      assert.equal(cardsOffConnections.length, 1, "one workspace stream");
      await page.setViewportSize({ width, height: 900 });
      if (width <= 760) {
        await page
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await page.getByRole("menuitem", { name: "Team", exact: true }).click();
      } else await page.locator("#team-toggle").click();
      const team = page.locator("#team");
      await team.waitFor();
      // Let the mobile drawer finish its entrance before geometry checks.
      await page.waitForTimeout(350);
      assert.equal(
        await team.getAttribute("class"),
        size === 3 ? "team-compact" : null,
      );
      const meter = (id) =>
        team.locator(`.token-rate[data-agent="${id}"][data-variant="worker"]`);
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
      await assertConnectionCount(
        page,
        cardsOffConnections,
        "Team cards without values add zero connections",
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
      await assertConnectionCount(
        page,
        cardsOffConnections,
        "Team cards with values add zero connections",
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
        await page.evaluate(
          () =>
            window.__teamRateSources.filter((source) => source.readyState !== 2)
              .length,
        ),
        1,
        "one shared workspace connection for all cards",
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
      follower = await page.context().newPage();
      await follower.addInitScript(() => {
        const Native = window.EventSource;
        window.__allSources = [];
        window.EventSource = class extends Native {
          constructor(...args) {
            super(...args);
            window.__allSources.push(this);
          }
        };
      });
      await follower.goto(origin);
      await follower.locator(`[data-chat="${lead.id}"]`).click();
      if (
        (await follower
          .locator("#team-toggle")
          .getAttribute("aria-expanded")) !== "true"
      )
        await follower.locator("#team-toggle").click();
      for (const agent of running)
        await follower.waitForFunction(
          (id) =>
            document
              .querySelector(`#team .token-rate[data-agent="${id}"]`)
              ?.textContent.includes("tok/s"),
          agent.id,
        );
      for (const agent of running)
        assert.doesNotMatch(
          await follower
            .locator(`#team .token-rate[data-agent="${agent.id}"]`)
            .innerText(),
          /^0 tok\/s$/,
        );
      await page.evaluate(() =>
        document.dispatchEvent(new Event("visibilitychange")),
      );
      await page.waitForFunction(
        () =>
          window.__teamRateSources.some((source) => source.readyState === 2) &&
          window.__teamRateSources.some((source) => source.readyState === 1),
      );
      await page.evaluate(() => {
        const stale = window.__teamRateSources.find(
          (source) => source.readyState === 2,
        );
        stale?.dispatchEvent(new Event("error"));
        if (!stale) throw Error("Expected the replaced workspace stream");
      });
      await page.waitForFunction(
        (id) =>
          document
            .querySelector(`#team .token-rate[data-agent="${id}"]`)
            ?.textContent.includes("tok/s"),
        running[0].id,
      );
      assert.equal(
        (await page.evaluate(
          () =>
            window.__allSources.filter(
              (source) =>
                source.readyState !== 2 && source.url.includes("protocol=3"),
            ).length,
        )) +
          (await follower.evaluate(
            () =>
              window.__allSources.filter(
                (source) =>
                  source.readyState !== 2 && source.url.includes("protocol=3"),
              ).length,
          )),
        1,
        "two tabs share the existing workspace coordinator for token rates",
      );
      assert.equal(
        await follower
          .locator(`#team .token-rate[data-agent="${idle.id}"]`)
          .innerText(),
        "",
      );
      const followerMetersOnConnections = await connections(follower);
      await follower.locator("#team-close").click();
      await assertConnectionCount(
        follower,
        followerMetersOnConnections,
        "the follower cards add zero connections",
      );
      await follower.close();
      follower = undefined;
      await page.waitForFunction(() =>
        window.__teamRateSources.some((source) => source.readyState === 1),
      );
      clearInterval(timer);
      timer = undefined;
      await page.clock.runFor(1000);
      const firstMeter = meter(running[0].id);
      const heldCardSample = await page.evaluate(
        (id) =>
          window.__teamRateBatches.findLast((batch) => batch.rates[id])?.rates[
            id
          ],
        running[0].id,
      );
      const beforeSilence = await firstMeter.innerText();
      const beforeSilenceRate = await firstMeter.getAttribute("data-rate");
      assert.match(beforeSilence, /tok\/s/);
      await page.clock.runFor(10000);
      assert.equal(await firstMeter.innerText(), beforeSilence);
      assert.equal(
        await firstMeter.getAttribute("data-rate"),
        beforeSilenceRate,
      );
      notify(running[0], "item/started", {
        item: {
          id: "tool-wait",
          type: "commandExecution",
          command: "wait",
        },
      });
      // Browser-clock advancement above checks client timers only. Keep 12.5
      // seconds of real fixture-server silence before probing its stream.
      await page.waitForTimeout(12500);
      const cardSampleAfterGap = await page.evaluate(
        (id) =>
          window.__teamRateBatches.findLast((batch) => batch.rates[id])?.rates[
            id
          ],
        running[0].id,
      );
      assert.equal(
        await firstMeter.getAttribute("data-rate"),
        beforeSilenceRate,
      );
      assert.equal(cardSampleAfterGap?.rate, heldCardSample?.rate);
      assert.equal(
        cardSampleAfterGap?.outputTokens,
        heldCardSample?.outputTokens,
      );
      assert.equal(await firstMeter.getAttribute("data-active"), "true");
      assert.equal(await firstMeter.innerText(), beforeSilence);
      clearInterval(timer);
      timer = undefined;
      const injectOnBatch = async (rate) =>
        page.evaluate(
          ({ teamId, ids, rate }) => {
            const source = window.__teamRateSources.findLast(
              (candidate) => candidate.readyState === 1,
            );
            const batch = window.__teamRateBatches.at(-1);
            if (!source || !batch) throw Error("No current token-rate batch");
            const revision =
              Math.max(batch.revision, window.__teamRateInjectedRevision) + 1;
            window.__teamRateInjectedRevision = revision;
            const rates = Object.fromEntries(
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
            );
            source.dispatchEvent(
              new MessageEvent("token-rates", {
                data: JSON.stringify({
                  ...batch,
                  revision,
                  test: true,
                  rates: { ...batch.rates, ...rates },
                  teams: { ...batch.teams, [teamId]: rates },
                }),
              }),
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
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.textContent === "20 tok/s",
        running[0].id,
      );
      await page.emulateMedia({ reducedMotion: "no-preference" });
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.dataset.reducedMotion === "false",
        running[0].id,
      );
      await injectOnBatch(80);
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.textContent === "80 tok/s",
        running[0].id,
      );
      await page.emulateMedia({ reducedMotion: "reduce" });
      await injectOnBatch(140);
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.textContent === "140 tok/s",
        running[0].id,
      );
      timer = setInterval(feed, 700);
      await page.waitForFunction(() => {
        const batch = window.__teamRateBatches.at(-1);
        return batch && batch.revision > window.__teamRateInjectedRevision;
      });
      notify(running[0], "turn/completed", {
        turn: { id: "fixture-turn", status: "completed" },
      });
      await page.waitForFunction(
        (id) =>
          document.querySelector(
            `.token-rate[data-agent="${id}"][data-variant="worker"]`,
          )?.dataset.active === "false" &&
          document
            .querySelector(
              `.token-rate[data-agent="${id}"][data-variant="worker"]`,
            )
            ?.textContent.includes("tok/s"),
        running[0].id,
      );
      clearInterval(timer);
      timer = undefined;
      assert.match(await meter(running[0].id).textContent(), /tok\/s/);
      await page.locator("#team-close").click();
      await page.waitForFunction(() =>
        window.__teamRateSources.some((source) => source.readyState === 1),
      );
      assert.equal(
        await page.evaluate(
          () =>
            window.__teamRateSources.filter((source) => source.readyState === 1)
              .length,
        ),
        1,
        "closing Team retains the existing shared sync stream",
      );
      assert.deepEqual(errors, []);
      console.log(
        `PASS ${size === 3 ? "compact" : "grouped"} Team cards, ${running.length} active workers, idle hidden, live rate snapshots, 1 connection, tween, reduced motion, stable ${width}px`,
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
      await follower?.close();
      await context.close();
    }
  }
});
