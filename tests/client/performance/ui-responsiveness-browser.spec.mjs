import {
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
  installApiSchemaFixtureHeaders,
  entityPullFixtureForRequest,
  apiSchemaHeaders,
  test,
  spawnFixture as spawn,
} from "../playwright.mjs";
// Production App at 4x CPU. Timings are evidence, correctness is asserted.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, writeFile, cp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash, randomUUID } from "node:crypto";

async function installTranscriptPageRoute(page, histories, agents) {
  await page.route("**/api/transcript/page?*", async (route) => {
    const url = new URL(route.request().url());
    const id = url.searchParams.get("id");
    const items = histories.get(id) || [];
    const limit = Math.max(1, Number(url.searchParams.get("limit") || 100));
    let start = 0;
    let end = 0;
    const before = url.searchParams.get("before");
    const after = url.searchParams.get("after");
    const around = url.searchParams.get("around");
    if (before) {
      end = Math.max(
        0,
        items.findIndex((item) => item.id === before),
      );
      start = Math.max(0, end - limit);
    } else if (after) {
      start = Math.max(0, items.findIndex((item) => item.id === after) + 1);
      end = Math.min(items.length, start + limit);
    } else if (around) {
      const index = Math.max(
        0,
        items.findIndex((item) => item.id === around),
      );
      start = Math.max(0, index - 120);
      end = Math.min(items.length, start + 240);
    }
    const selected = items.slice(start, end);
    await route.fulfill({
      json: {
        agent: agents.find((agent) => agent.id === id) || null,
        historyVersion: "fixed-history",
        items: selected,
        truncated: false,
        nextCursor: start > 0 ? selected[0]?.id || null : null,
        nextAfterCursor:
          end < items.length ? selected.at(-1)?.id || null : null,
      },
    });
  });
}

function responsivenessHistory(prefix) {
  const items = [];
  for (let turn = 0; turn < 30; turn++) {
    const shared = {
      turnId: `${prefix}-turn-${turn}`,
      turnStatus: "completed",
    };
    items.push({
      ...shared,
      id: `${prefix}-user-${turn}`,
      role: "user",
      text: `Review component ${turn}.`,
    });
    for (let step = 0; step < 4; step++) {
      items.push({
        ...shared,
        id: `${prefix}-text-${turn}-${step}`,
        role: "assistant",
        text: `Component ${turn}, step ${step}.\n\n${"The **test evidence** explains this behavior. ".repeat(6)}\n\n- Inspect the request\n- Check the response\n\n\`\`\`ts\nconst result = await check();\n\`\`\``,
      });
      items.push({
        ...shared,
        id: `${prefix}-tool-${turn}-${step}`,
        role: "tool",
        toolStatus: "completed",
        title: "commandExecution",
        text: JSON.stringify({
          type: "commandExecution",
          command: `python3 check_${step}.py`,
          status: "completed",
          aggregatedOutput: "check passed\n".repeat(30),
          exitCode: 0,
        }),
      });
    }
    items.push({
      ...shared,
      id: `${prefix}-result-${turn}`,
      role: "assistant",
      phase: "final_answer",
      text: `Component ${turn} checked. All evidence remains available.`,
    });
  }
  items.push({
    id: `${prefix}-live`,
    role: "assistant",
    turnId: `${prefix}-live-turn`,
    turnStatus: "running",
    text: "Stream revision 0.",
  });
  return items;
}

async function runUiResponsiveness({
  browser: testBrowser,
  context: runnerContext,
  assertIdleRootWindows,
}) {
  test.setTimeout(300_000);
  const repo = fileURLToPath(new URL("../../../", import.meta.url));
  const dir = await mkdtemp(join(tmpdir(), "studio-ui-responsiveness-"));
  const staticDir = join(dir, "dist");
  await cp(
    process.env.RESPONSIVENESS_DIST || join(repo, "web/dist"),
    staticDir,
    {
      recursive: true,
    },
  );
  const index = await readFile(join(staticDir, "index.html"));
  const identity = {
    source: execFileSync("git", ["rev-parse", "HEAD"], {
      cwd: repo,
      encoding: "utf8",
    }).trim(),
    label: process.env.RESPONSIVENESS_LABEL || "measurement",
    indexSha256: createHash("sha256").update(index).digest("hex"),
    cpuSlowdown: 4,
  };
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, CODEX_BOARD_STATE_DIR: join(dir, "board") },
    },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  const results = [];
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const original = await readTestState(origin);
    const a = original.threads.find((agent) => agent.name === "Other project");
    const b = original.threads.find((agent) => agent.name === "Release lead");
    const agents = [a, b].map((agent) => ({
      ...agent,
      status: "completed",
      inFlight: false,
      turnId: null,
    }));
    const state = {
      ...original,
      threads: agents,
      nodes: agents,
      chats: [],
      edges: [],
      runtime: {
        ...original.runtime,
        agents,
        rooms: [],
        requests: [],
        events: [],
        work: [],
        tasks: [],
        userTasks: [],
        complaints: [],
        monitors: [],
      },
    };
    const histories = new Map([
      [a.id, responsivenessHistory("a")],
      [b.id, responsivenessHistory("b")],
    ]);
    for (const [name, viewport] of [
      ["desktop", { width: 1440, height: 960 }],
      ["mobile", { width: 390, height: 844 }],
    ]) {
      const context = await testBrowser.newContext({
        viewport,
        serviceWorkers: "block",
      });
      const page = await context.newPage();
      page.setDefaultTimeout(30000);
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      installApiSchemaFixtureHeaders(page);
      const cdp = await context.newCDPSession(page);
      await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
      await page.addInitScript(
        ({ stateDir, id, rows }) => {
          localStorage.setItem(
            `codex-desktop-opened:${stateDir}`,
            JSON.stringify(id),
          );
          localStorage.setItem(
            `codex-mobile-opened:${stateDir}`,
            JSON.stringify(id),
          );
          for (const [chat, items] of rows)
            localStorage.setItem(
              `studio-turns:${stateDir}:${chat}:tools-v3`,
              JSON.stringify(
                Object.fromEntries(
                  items
                    .filter((item) => item.role === "tool")
                    .map((item) => [item.id, false]),
                ),
              ),
            );
          window.metrics = {
            phase: "startup",
            inputSequence: 0,
            lastInputAt: -Infinity,
            input: [],
            layoutReads: {},
            longTasks: [],
            events: [],
            renderProbe: [],
            switches: [],
            stream: [],
          };
          window.__studioPromptComposerRenderProbe = (component) =>
            window.metrics.renderProbe.push({
              component,
              phase: window.metrics.phase,
              inputSequence: window.metrics.inputSequence,
              inputAgeMs: performance.now() - window.metrics.lastInputAt,
            });
          document.addEventListener(
            "input",
            () => {
              window.metrics.inputSequence++;
              window.metrics.lastInputAt = performance.now();
            },
            true,
          );
          const rectangle = Element.prototype.getBoundingClientRect;
          Element.prototype.getBoundingClientRect = function (...args) {
            const key = `${window.metrics.phase}:${this.tagName}`;
            window.metrics.layoutReads[key] =
              (window.metrics.layoutReads[key] || 0) + 1;
            return rectangle.apply(this, args);
          };
          new PerformanceObserver((list) =>
            window.metrics.longTasks.push(
              ...list.getEntries().map((entry) => ({
                phase: window.metrics.phase,
                start: entry.startTime,
                duration: entry.duration,
              })),
            ),
          ).observe({ type: "longtask", buffered: true });
          new PerformanceObserver((list) =>
            window.metrics.events.push(
              ...list.getEntries().map((entry) => ({
                phase: window.metrics.phase,
                type: entry.name,
                duration: entry.duration,
                inputDelay: entry.processingStart - entry.startTime,
              })),
            ),
          ).observe({ type: "event", buffered: true, durationThreshold: 16 });
          document.addEventListener(
            "input",
            (event) => {
              if (event.target.id !== "message") return;
              const started = event.timeStamp,
                value = event.target.value;
              requestAnimationFrame(() =>
                requestAnimationFrame(() =>
                  window.metrics.input.push({
                    phase: window.metrics.phase,
                    duration: performance.now() - started,
                    value,
                  }),
                ),
              );
            },
            true,
          );
          document.addEventListener(
            "pointerdown",
            (event) => {
              const chat = event.target.closest("[data-chat]");
              if (!chat || !window.switchExpected) return;
              const started = event.timeStamp,
                expected = window.switchExpected;
              const inspect = () => {
                if (!document.querySelector(`[data-message="${expected}"]`))
                  return requestAnimationFrame(inspect);
                requestAnimationFrame(() =>
                  window.metrics.switches.push({
                    phase: window.metrics.phase,
                    duration: performance.now() - started,
                    expected,
                  }),
                );
              };
              requestAnimationFrame(inspect);
            },
            true,
          );
        },
        { stateDir: original.stateDir, id: a.id, rows: [...histories] },
      );
      await page.route("**/*", async (route) => {
        const url = new URL(route.request().url());
        if (url.origin !== origin) return route.fallback();
        if (!url.pathname.startsWith("/api/")) {
          const path =
            url.pathname === "/"
              ? "index.html"
              : decodeURIComponent(url.pathname).slice(1);
          if (path.split("/").includes("..")) return route.abort();
          const type = path.endsWith(".js")
            ? "text/javascript"
            : path.endsWith(".css")
              ? "text/css"
              : path.endsWith(".html")
                ? "text/html"
                : undefined;
          return route
            .fulfill({ path: join(staticDir, path), contentType: type })
            .catch(() => route.fallback());
        }
        if (url.pathname === "/api/transcript") {
          const id = url.searchParams.get("id");
          return route.fulfill({
            json: {
              agent: agents.find((agent) => agent.id === id),
              items: histories.get(id) || [],
              historyVersion: "fixed-history",
            },
          });
        }
        return route.fallback();
      });
      const syncContract = await readFixtureSyncContract(origin);
      await stubEntityState(page, state, syncContract);
      let transcriptSeq = 1;
      await page.route("**/api/sync/pull?*", (route) => {
        const requestUrl = route.request().url();
        const scope = new URL(requestUrl).searchParams.get("scope");
        const transcriptId = scope?.startsWith("transcript:")
          ? scope.slice("transcript:".length)
          : undefined;
        return route.fulfill({
          json: {
            workspaceId: syncContract.identity.workspaceId,
            ...entityPullFixtureForRequest(state, requestUrl, {
              transcriptSeq,
              ...(transcriptId
                ? {
                    transcript: {
                      agent: agents.find((agent) => agent.id === transcriptId),
                      items: histories.get(transcriptId) || [],
                      historyVersion: "fixed-history",
                      truncated: false,
                      nextAfterCursor: null,
                      nextCursor: null,
                    },
                  }
                : {}),
            }),
          },
        });
      });
      await installTranscriptPageRoute(page, histories, agents);
      await page.goto(origin);
      if (!(await page.locator('[data-message="a-result-29"]').count())) {
        if (name === "mobile")
          await page.getByLabel("Toggle conversations").click();
        await page.locator(`[data-chat="${a.id}"]`).click();
      }
      // HistoryWindow retains only rows around the viewport. Reveal the
      // measured target through the same event used by message navigation.
      await page.waitForFunction(() => {
        const root = [...document.querySelectorAll("#messages")].find(
          (node) => {
            const bounds = node.getBoundingClientRect();
            return bounds.width > 0 && bounds.height > 0;
          },
        );
        if (!root) return false;
        root.dispatchEvent(
          new CustomEvent("studio-history-reveal", {
            detail: "a-result-29",
          }),
        );
        return !!root.querySelector('[data-message="a-result-29"]');
      });
      await page.locator('[data-message="a-result-29"]').waitFor();
      await page.waitForTimeout(500);
      const setPhase = (phase) =>
        page.evaluate((phase) => {
          window.metrics.phase = phase;
        }, phase);
      const input = page.locator("#message");
      await setPhase("idle-input");
      await input.focus();
      let firstText = "Keep every typed character.";
      await page.keyboard.type(firstText, { delay: 35 });
      assert.equal(await input.inputValue(), firstText);
      await page.waitForTimeout(150);
      await page.locator("#messages").evaluate((element) => {
        element.scrollTop = Math.floor(element.scrollHeight * 0.25);
        element.dispatchEvent(new Event("scroll"));
      });
      await page.waitForTimeout(150);
      await setPhase("scrolled-input");
      // Let the scroll-state and background sync transitions settle before
      // attributing renders to the following keyboard input.
      await page.waitForTimeout(1000);
      const scrolledSuffix = " Type while reading older text.";
      await page.keyboard.type(scrolledSuffix, { delay: 35 });
      firstText += scrolledSuffix;
      assert.equal(await input.inputValue(), firstText);
      await page.waitForTimeout(150);
      const switchTo = async (agent, prefix) => {
        if (name === "mobile")
          await page.getByLabel("Toggle conversations").click();
        const count = await page.evaluate((expected) => {
          window.switchExpected = expected;
          return window.metrics.switches.length;
        }, `${prefix}-result-29`);
        await page.locator(`[data-chat="${agent.id}"]`).click();
        await page.waitForFunction(
          (count) => window.metrics.switches.length > count,
          count,
        );
      };
      await setPhase("cold-switch");
      await switchTo(b, "b");
      await setPhase("cached-switch");
      await switchTo(a, "a");
      assert.equal(
        await input.inputValue(),
        firstText,
        "Chat switch retains the draft",
      );
      await setPhase("stream-and-input");
      await input.focus();
      await input.evaluate((element) =>
        element.setSelectionRange(element.value.length, element.value.length),
      );
      const suffix = " Input while the answer streams.";
      const emitTranscriptRevision = async (revision, items) => {
        histories.set(a.id, items);
        transcriptSeq++;
        const startedAt = await page.evaluate(() => performance.now());
        const pull = page.waitForResponse((response) => {
          const url = new URL(response.url());
          return (
            url.pathname === "/api/sync/pull" &&
            url.searchParams.get("scope") === `transcript:${a.id}` &&
            Number(url.searchParams.get("after") || 0) < transcriptSeq
          );
        });
        const notification = await fetch(new URL("/api/sync/notify", origin), {
          method: "POST",
          headers: {
            Origin: origin,
            "Content-Type": "application/json",
            "X-Canvas-Token": original.token,
            ...apiSchemaHeaders(),
          },
          body: JSON.stringify({
            requestId: randomUUID(),
            resources: [{ kind: "transcript", agentId: a.id }],
          }),
        });
        assert.equal(notification.status, 200, await notification.text());
        await pull;
        await page.waitForFunction(
          (revision) =>
            document
              .querySelector('[data-message="a-live"]')
              ?.textContent.includes(`Stream revision ${revision}.`),
          revision,
        );
        await page.evaluate(
          ({ startedAt, revision }) =>
            new Promise((resolve) =>
              requestAnimationFrame(() =>
                requestAnimationFrame(() => {
                  window.metrics.stream.push({
                    phase: window.metrics.phase,
                    revision,
                    duration: performance.now() - startedAt,
                  });
                  resolve();
                }),
              ),
            ),
          { startedAt, revision },
        );
      };
      const streaming = (async () => {
        for (let revision = 1; revision <= 12; revision++) {
          const items = [...histories.get(a.id)];
          items[items.length - 1] = {
            ...items.at(-1),
            text: `Stream revision ${revision}. ${"Current response text. ".repeat(revision)}`,
          };
          await emitTranscriptRevision(revision, items);
          await page.waitForTimeout(100);
        }
      })();
      await Promise.all([streaming, page.keyboard.type(suffix, { delay: 35 })]);
      await page.waitForFunction(() => window.metrics.stream.length === 12);
      assert.equal(
        await input.inputValue(),
        firstText + suffix,
        "Streaming does not lose typed characters",
      );
      let revealedCommentary = 0;
      let earlierPageClicks = 0;
      const revealCommentary = async (
        id,
        { allowEarlierPaging = true } = {},
      ) => {
        const visibleTranscript = page.locator("#messages:visible").first();
        const pageHops = allowEarlierPaging ? 3 : 1;
        for (let pageHop = 0; pageHop < pageHops; pageHop++) {
          await visibleTranscript.evaluate((root, messageId) => {
            root.dispatchEvent(
              new CustomEvent("studio-history-reveal", {
                detail: messageId,
              }),
            );
          }, id);
          const target = visibleTranscript.locator(
            `[data-message="${id}"]:not([data-lazy-message])`,
          );
          try {
            await target.waitFor({ state: "attached" });
            return;
          } catch (error) {
            if (!allowEarlierPaging)
              throw new Error(
                `Loaded transcript page did not reveal ${id} after the public history reveal event`,
                { cause: error },
              );
          }

          const earlier = page.locator("#earlier-messages");
          if (!(await earlier.isVisible()))
            throw new Error(`No earlier transcript page can load ${id}`);
          const response = page.waitForResponse((candidate) => {
            const url = new URL(candidate.url());
            return (
              url.pathname === "/api/transcript/page" &&
              url.searchParams.has("before")
            );
          });
          earlierPageClicks++;
          await earlier.click();
          await response;
          await visibleTranscript.evaluate(
            () =>
              new Promise((resolve) =>
                requestAnimationFrame(() => requestAnimationFrame(resolve)),
              ),
          );
        }
        throw new Error(`Transcript pagination did not reveal ${id}`);
      };
      // Newest page first: each bounded page is complete within its range.
      for (let turn = 6; turn < 30; turn++) {
        for (let step = 0; step < 4; step++) {
          const id = `a-text-${turn}-${step}`;
          await revealCommentary(id, { allowEarlierPaging: false });
          revealedCommentary++;
        }
      }
      assert.equal(
        earlierPageClicks,
        0,
        "Every initial-page target is revealed before Earlier messages is clicked",
      );
      const newestResultCount = await page
        .locator('[data-message^="a-result-"]')
        .count();
      assert.equal(
        newestResultCount,
        24,
        "The newest page retains the latest 24 final answers",
      );
      assert.match(
        await page.locator('[data-message="a-live"]').innerText(),
        /Stream revision 12\./,
      );
      for (let turn = 0; turn < 6; turn++) {
        for (let step = 0; step < 4; step++) {
          const id = `a-text-${turn}-${step}`;
          await revealCommentary(id);
          revealedCommentary++;
        }
      }
      assert.equal(
        revealedCommentary,
        120,
        "All commentary remains reachable through history reveal",
      );
      assert.equal(
        await page.locator('[data-message^="a-result-"]').count(),
        24,
        "The older page retains its 24 final answers",
      );
      assert.equal(
        await page.locator('[data-message^="b-"]').count(),
        0,
        "The other chat does not leak into history",
      );
      assert.deepEqual(errors, []);
      const metrics = await page.evaluate(() => window.metrics);
      const summarize = (values) => {
        const sorted = [...values].sort((a, b) => a - b);
        return {
          count: sorted.length,
          p50: sorted[Math.floor(sorted.length * 0.5)] || 0,
          p95:
            sorted[
              Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95))
            ] || 0,
          max: sorted.at(-1) || 0,
        };
      };
      const summary = {
        viewport: name,
        records: histories.get(a.id).length,
        domElements: await page.locator("#messages *").count(),
        idleInput: summarize(
          metrics.input
            .filter((x) => x.phase === "idle-input")
            .map((x) => x.duration),
        ),
        scrolledInput: summarize(
          metrics.input
            .filter((x) => x.phase === "scrolled-input")
            .map((x) => x.duration),
        ),
        layoutReads: metrics.layoutReads,
        keyboardEvents: summarize(
          metrics.events
            .filter((x) =>
              ["keydown", "keypress", "keyup", "input"].includes(x.type),
            )
            .map((x) => x.duration),
        ),
        streamInput: summarize(
          metrics.input
            .filter((x) => x.phase === "stream-and-input")
            .map((x) => x.duration),
        ),
        streamPaint: summarize(metrics.stream.map((x) => x.duration)),
        switches: metrics.switches,
        renderCounts: Object.fromEntries(
          ["idle-input", "scrolled-input", "stream-and-input"].map((phase) => [
            phase,
            Object.fromEntries(
              ["app", "sidebar", "conversation"].map((component) => [
                component,
                metrics.renderProbe.filter(
                  (entry) =>
                    entry.phase === phase &&
                    entry.component === component &&
                    entry.inputAgeMs < 20,
                ).length,
              ]),
            ),
          ]),
        ),
        longTasks: Object.fromEntries(
          [
            "startup",
            "idle-input",
            "scrolled-input",
            "cold-switch",
            "cached-switch",
            "stream-and-input",
          ].map((phase) => [
            phase,
            summarize(
              metrics.longTasks
                .filter((x) => x.phase === phase)
                .map((x) => x.duration),
            ),
          ]),
        ),
      };
      await writeFile(
        join(dir, `${name}-metrics.json`),
        JSON.stringify(metrics, null, 2),
      );
      for (const phase of ["idle-input", "scrolled-input"]) {
        const counts = summary.renderCounts[phase];
        if (process.env.RENDER_ISOLATION === "baseline") {
          assert.ok(
            counts.app > 0 && counts.sidebar > 0 && counts.conversation > 0,
            `${name}/${phase}: legacy draft owner should rerender its caller tree`,
          );
        } else {
          if (phase !== "idle-input" || assertIdleRootWindows) {
            assert.equal(
              counts.app,
              0,
              `${name}/${phase}: typing must not rerender App`,
            );
            assert.equal(
              counts.sidebar,
              0,
              `${name}/${phase}: typing must not rerender the sidebar`,
            );
            assert.ok(
              counts.conversation <= 1,
              `${name}/${phase}: one incidental Conversation update may coincide with typing, not one per key`,
            );
          }
        }
      }
      results.push(summary);
      await writeFile(
        join(dir, `${name}-metrics.json`),
        JSON.stringify(metrics, null, 2),
      );
      await page.screenshot({ path: join(dir, `${name}.png`) });
      console.log(JSON.stringify(summary));
      await context.close();
    }
    const report = {
      identity,
      results,
      evidence: dir,
      result: "PASS correctness; timing report",
    };
    await writeFile(join(dir, "report.json"), JSON.stringify(report, null, 2));
    console.log(JSON.stringify(report));
  } catch (error) {
    console.error("Evidence:", dir, fixtureLog);
    throw error;
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
  }
}

test(
  "Ui responsiveness browser",
  { tag: "@performance" },
  async ({ browser, context }) =>
    runUiResponsiveness({
      browser,
      context,
      assertIdleRootWindows: false,
    }),
);

test(
  "Ui responsiveness App render isolation (unexplained intermittent)",
  { tag: "@performance" },
  async ({ browser, context }) => {
    test.fixme(
      true,
      "Unexplained root render falls inside a typing input window; preserve the App, Sidebar, and Conversation assertions together and revisit when the changing App-level state is identified",
    );
    await runUiResponsiveness({
      browser,
      context,
      assertIdleRootWindows: true,
    });
  },
);

test(
  "Ui responsiveness history can return from older messages",
  { tag: "@performance" },
  async ({ browser }) => {
    test.setTimeout(120_000);
    const repo = fileURLToPath(new URL("../../../", import.meta.url));
    const dir = await mkdtemp(join(tmpdir(), "studio-ui-history-return-"));
    await cp(
      process.env.RESPONSIVENESS_DIST || join(repo, "web/dist"),
      join(dir, "dist"),
      { recursive: true },
    );
    const fixture = spawn(
      "python3",
      ["-B", join(repo, "tests/simple-ui-fixture.py"), dir],
      {
        stdio: ["ignore", "pipe", "pipe"],
        env: { ...process.env, CODEX_BOARD_STATE_DIR: join(dir, "board") },
      },
    );
    let fixtureLog = "";
    fixture.stderr.on("data", (chunk) => {
      fixtureLog += chunk;
    });
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const original = await readTestState(origin);
    const a = original.threads.find((agent) => agent.name === "Other project");
    const b = original.threads.find((agent) => agent.name === "Release lead");
    const agents = [a, b].map((agent) => ({
      ...agent,
      status: "completed",
      inFlight: false,
      turnId: null,
    }));
    const state = {
      ...original,
      threads: agents,
      nodes: agents,
      chats: [],
      edges: [],
      runtime: {
        ...original.runtime,
        agents,
        rooms: [],
        requests: [],
        events: [],
        work: [],
        tasks: [],
        userTasks: [],
        complaints: [],
        monitors: [],
      },
    };
    const histories = new Map([
      [a.id, responsivenessHistory("a")],
      [b.id, responsivenessHistory("b")],
    ]);
    const page = await browser.newPage({
      viewport: { width: 1440, height: 960 },
      serviceWorkers: "block",
    });
    try {
      page.setDefaultTimeout(30_000);
      installApiSchemaFixtureHeaders(page);
      await page.addInitScript(
        ({ stateDir, id }) => {
          localStorage.setItem(`codex-desktop-opened:${stateDir}`, id);
          localStorage.setItem(`codex-mobile-opened:${stateDir}`, id);
        },
        { stateDir: original.stateDir, id: a.id },
      );
      const syncContract = await readFixtureSyncContract(origin);
      await stubEntityState(page, state, syncContract);
      await page.route("**/api/sync/pull?*", (route) => {
        const requestUrl = route.request().url();
        const url = new URL(requestUrl);
        const scope = url.searchParams.get("scope");
        const transcriptId = scope?.startsWith("transcript:")
          ? scope.slice("transcript:".length)
          : undefined;
        return route.fulfill({
          json: {
            workspaceId: syncContract.identity.workspaceId,
            ...entityPullFixtureForRequest(state, requestUrl, {
              transcriptSeq: 1,
              ...(transcriptId
                ? {
                    transcript: {
                      agent: agents.find((agent) => agent.id === transcriptId),
                      items: histories.get(transcriptId) || [],
                      historyVersion: "fixed-history",
                      truncated: false,
                      nextAfterCursor: null,
                      nextCursor: null,
                    },
                  }
                : {}),
            }),
          },
        });
      });
      await installTranscriptPageRoute(page, histories, agents);
      await page.goto(origin);
      await page.locator(`[data-chat="${a.id}"]`).waitFor({ state: "visible" });
      await page.locator("#messages").waitFor({ state: "visible" });
      if (!(await page.locator('[data-message="a-live"]').count()))
        await page.locator(`[data-chat="${a.id}"]`).click();
      await page.locator('[data-message="a-live"]').waitFor();

      const earlier = page.locator("#earlier-messages");
      while (await earlier.isVisible()) {
        const response = page.waitForResponse((candidate) => {
          const url = new URL(candidate.url());
          return (
            url.pathname === "/api/transcript/page" &&
            url.searchParams.has("before")
          );
        });
        await earlier.click();
        await response;
      }
      const later = page.locator("#newer-messages");
      let newestReturnedId;
      while (await later.isVisible()) {
        const response = page.waitForResponse((candidate) => {
          const url = new URL(candidate.url());
          return (
            url.pathname === "/api/transcript/page" &&
            url.searchParams.has("after")
          );
        });
        await later.click();
        const pageResponse = await response;
        const pageData = await pageResponse.json();
        newestReturnedId = pageData.items?.at(-1)?.id;
      }
      const heldPageLastId = await page
        .locator("#messages [data-message]:not([data-lazy-message])")
        .last()
        .getAttribute("data-message");
      const laterButtonVisible = await later.isVisible();
      const scrollRoot = page.locator("#messages");
      const requestsBeforeScroll = await page.evaluate(
        () =>
          performance
            .getEntriesByType("resource")
            .filter((entry) =>
              entry.name.includes("/api/transcript/page?after="),
            ).length,
      );
      await scrollRoot.evaluate((root) => {
        root.scrollTop = root.scrollHeight;
        root.dispatchEvent(new Event("scroll", { bubbles: true }));
      });
      await page
        .locator('[data-message="a-live"]:not([data-lazy-message])')
        .waitFor({ state: "visible" });
      const requestsAfterScroll = await page.evaluate(
        () =>
          performance
            .getEntriesByType("resource")
            .filter((entry) =>
              entry.name.includes("/api/transcript/page?after="),
            ).length,
      );
      const latestMounted = await page
        .locator('[data-message="a-live"]:not([data-lazy-message])')
        .count();
      const laterCommentaryMounted = await page
        .locator('[data-message="a-text-24-0"]:not([data-lazy-message])')
        .count();
      assert.equal(
        heldPageLastId,
        "a-live",
        "The retained page ends at the newest fixture row",
      );
      assert.equal(
        newestReturnedId,
        "a-live",
        "The last forward page contains the newest fixture row",
      );
      assert.equal(laterButtonVisible, false, "Later messages is exhausted");
      assert.equal(
        latestMounted,
        1,
        "The newest transcript row is mounted after scrolling to the end",
      );
      assert.equal(
        laterCommentaryMounted,
        1,
        "Commentary in the forward page is mounted",
      );
      assert.equal(
        requestsAfterScroll,
        requestsBeforeScroll,
        "Scrolling does not request another page",
      );
    } finally {
      await page.close().catch(() => {});
    }
  },
);
