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
      viewport: { width, height: 900 },
    });
    const page = await context.newPage();
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
      await page.addInitScript(() => {
        const Native = window.EventSource;
        window.__allSources = [];
        window.__teamRateSources = [];
        window.__teamRateBatches = [];
        window.EventSource = class extends Native {
          constructor(...args) {
            super(...args);
            window.__allSources.push(this);
            Native.prototype.addEventListener.call(
              this,
              "resources",
              (event) => {
                const frame = JSON.parse(event.data);
                if (frame.reason === "initial") this.resourceBaseline = frame;
              },
            );
            if (this.url.includes("/api/sync/stream?protocol=3")) {
              window.__teamRateSources.push(this);
              this.addEventListener("token-rates", (event) => {
                const batch = JSON.parse(event.data);
                if (!batch.test)
                  window.__teamRateBatches.push({
                    at: performance.now(),
                    ...batch,
                    rates: batch.teams[window.__rateTeamId] || {},
                  });
              });
            }
          }
        };
      });
      await page.addInitScript((id) => {
        window.__rateTeamId = id;
      }, lead.id);
      await page.goto(origin);
      if (width <= 760) await page.locator("#sidebar-toggle").click();
      await page.locator(`[data-chat="${lead.id}"]`).click();
      const waitForResourceBaseline = () =>
        page.waitForFunction(
          ({ leadId, transcriptCount }) => {
            const source = window.__allSources.at(-1);
            const transcripts = source?.resourceBaseline?.resources.filter(
              (resource) => resource.kind === "transcript",
            );
            return (
              source?.readyState === EventSource.OPEN &&
              transcripts?.length === transcriptCount &&
              transcripts.some((resource) => resource.agentId === leadId)
            );
          },
          {
            leadId: lead.id,
            transcriptCount: width <= 760 ? 3 : agents.length,
          },
        );
      await waitForResourceBaseline();
      const connections = (view) =>
        view.evaluate(() =>
          window.__allSources
            .filter((source) => source.readyState !== 2)
            .map((source) => source.url)
            .sort(),
        );
      const cardsOffConnections = await connections(page);
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
      await card(idle.id)
        .locator(".worker-status-accessible small")
        .filter({ hasText: /^Waiting$/ })
        .waitFor({ state: "attached" });
      assert.match(
        await card(idle.id).locator(".worker-state").getAttribute("aria-label"),
        /Turn ended/,
      );
      await waitForResourceBaseline();
      assert.deepEqual(
        await connections(page),
        cardsOffConnections,
        "Team cards without values add zero connections",
      );
      const showMeter = async (id) => {
        await card(id).scrollIntoViewIfNeeded();
        await page.waitForFunction(
          (id) =>
            document.querySelector(
              `#team .token-rate[data-agent="${id}"][data-variant="worker"]`,
            )?.dataset.visualActive === "true",
          id,
        );
      };
      await showMeter(running[0].id);
      const before = await card(running[0].id).boundingBox();
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
      for (const agent of running) {
        await showMeter(agent.id);
        await page.waitForFunction(
          (id) =>
            document
              .querySelector(
                `.token-rate[data-agent="${id}"][data-variant="worker"]`,
              )
              ?.textContent.includes("tok/s"),
          agent.id,
        );
      }
      await showMeter(running[0].id);
      assert.deepEqual(
        await connections(page),
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
      assert.ok(
        afterMeter.x >= after.x &&
          afterMeter.x + afterMeter.width <= after.x + after.width &&
          afterMeter.y >= after.y &&
          afterMeter.y + afterMeter.height <= after.y + after.height,
        "the visible rate stays inside the unchanged card",
      );
      const modelBounds = await card(running[0].id)
        .locator(".worker-model-summary")
        .boundingBox();
      assert.ok(
        modelBounds.x + modelBounds.width <= afterMeter.x,
        "the visible rate does not overlap the model label",
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
          `at most one batch per second: ${JSON.stringify(batches.map((batch) => batch.at))}`,
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
      await follower.setViewportSize({ width: 1200, height: 900 });
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
      for (const agent of running) {
        await follower
          .locator(`#team [data-worker="${agent.id}"]`)
          .scrollIntoViewIfNeeded();
        await follower.waitForFunction(
          (id) =>
            document
              .querySelector(`#team .token-rate[data-agent="${id}"]`)
              ?.textContent.includes("tok/s"),
          agent.id,
        );
      }
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
      assert.deepEqual(
        await connections(follower),
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
      await page.waitForTimeout(1000);
      await showMeter(running[0].id);
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
      await page.waitForTimeout(10000);
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
      await page.waitForTimeout(1500);
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
      timer = setInterval(feed, 700);
      const injectOnBatch = async (rate) =>
        page.evaluate(
          async ({ teamId, ids, rate }) => {
            const source = window.__teamRateSources.at(-1);
            await new Promise((resolve) =>
              source.addEventListener(
                "token-rates",
                (event) => {
                  const batch = JSON.parse(event.data);
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
                  window.__syntheticRateRevision =
                    Math.max(
                      batch.revision,
                      window.__syntheticRateRevision || 0,
                    ) + 1;
                  source.dispatchEvent(
                    new MessageEvent("token-rates", {
                      data: JSON.stringify({
                        ...batch,
                        revision: window.__syntheticRateRevision,
                        test: true,
                        rates: { ...batch.rates, ...rates },
                        teams: { ...batch.teams, [teamId]: rates },
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
      clearInterval(timer);
      timer = undefined;
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
      assert.match(await meter(running[0].id).textContent(), /tok\/s/);
      const finalMeter = await meter(running[0].id).boundingBox();
      assert.equal(
        finalMeter.y,
        afterMeter.y,
        "rate updates keep the same row",
      );
      await page.locator("#team-close").click();
      assert.equal(
        await page.evaluate(
          () =>
            window.__teamRateSources.filter((source) => source.readyState === 1)
              .length,
        ),
        1,
        "closing Team retains the existing shared sync stream",
      );
      const nextWorker = running[1];
      const hiddenTokens = await page.evaluate(
        (id) => window.__teamRateBatches.at(-1).rates[id].outputTokens,
        nextWorker.id,
      );
      notify(nextWorker, "item/agentMessage/delta", {
        itemId: "answer",
        delta: "Output while Team is closed.",
      });
      await page.waitForFunction(
        ({ id, previousTokens }) =>
          window.__teamRateBatches.at(-1)?.rates[id]?.outputTokens >
          previousTokens,
        { id: nextWorker.id, previousTokens: hiddenTokens },
      );
      if (width <= 760) {
        await page
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await page.getByRole("menuitem", { name: "Team", exact: true }).click();
      } else await page.locator("#team-toggle").click();
      await showMeter(nextWorker.id);
      await page.waitForFunction((id) => {
        const sample = window.__teamRateBatches.at(-1)?.rates[id];
        const meter = document.querySelector(
          `#team .token-rate[data-agent="${id}"]`,
        );
        return sample?.rate > 0 && Number(meter?.dataset.rate) === sample.rate;
      }, nextWorker.id);
      assert.match(await meter(nextWorker.id).innerText(), /tok\/s/);
      assert.deepEqual(
        await connections(page),
        cardsOffConnections,
        "reopening Team uses the current cached rates and the same subscription",
      );
      assert.deepEqual(errors, []);
      console.log(
        `PASS ${size === 3 ? "compact" : "grouped"} Team cards, ${running.length} active workers, idle hidden, 1 batch/s, 0 added connections, 1 coordinator across 2 tabs, tween, reduced motion, stable ${width}px`,
      );
    } catch (error) {
      console.error(
        await page.evaluate(() => ({
          hidden: document.hidden,
          rootActive: document.documentElement.dataset.visualActive,
          teamBounds: document
            .querySelector("#team")
            ?.getBoundingClientRect()
            .toJSON(),
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
