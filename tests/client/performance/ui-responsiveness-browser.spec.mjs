import { test } from "../playwright.mjs";
// Production App at 4x CPU. Timings are evidence, correctness is asserted.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { spawnFixture as spawn } from "../playwright.mjs";
import { mkdtemp, readFile, writeFile, cp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
test(
  "Ui responsiveness browser",
  { tag: "@performance" },
  async ({ browser: testBrowser, context: runnerContext }) => {
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
      const original = await (await fetch(origin + "/api/state")).json();
      const a = original.threads.find(
        (agent) => agent.name === "Other project",
      );
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
      const history = (prefix) => {
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
      };
      const histories = new Map([
        [a.id, history("a")],
        [b.id, history("b")],
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
            window.fixtureStreams = [];
            window.EventSource = class extends EventTarget {
              constructor(url) {
                super();
                this.url = url;
                window.fixtureStreams.push(this);
              }
              close() {
                window.fixtureStreams = window.fixtureStreams.filter(
                  (stream) => stream !== this,
                );
              }
            };
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
            window.emitFixture = (id, packet, revision) => {
              const stream = window.fixtureStreams.find(
                (stream) =>
                  new URL(stream.url, location.href).searchParams.get("id") ===
                  id,
              );
              if (!stream)
                throw new Error("The selected transcript stream is missing");
              const started = performance.now();
              const inspect = () => {
                if (
                  !document
                    .querySelector('[data-message="a-live"]')
                    ?.textContent.includes(`Stream revision ${revision}.`)
                )
                  return requestAnimationFrame(inspect);
                requestAnimationFrame(() =>
                  window.metrics.stream.push({
                    phase: window.metrics.phase,
                    revision,
                    duration: performance.now() - started,
                  }),
                );
              };
              requestAnimationFrame(inspect);
              stream.onmessage({ data: JSON.stringify(packet) });
            };
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
          if (url.pathname.startsWith("/api/sync/"))
            return route.fulfill({
              status: 404,
              json: { error: "Controlled legacy transcript fixture" },
            });
          if (url.pathname === "/api/state")
            return route.fulfill({ json: state });
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
        await page.goto(origin);
        if (!(await page.locator('[data-message="a-result-29"]').count())) {
          if (name === "mobile")
            await page.getByLabel("Toggle conversations").click();
          await page.locator(`[data-chat="${a.id}"]`).click();
        }
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
        const streaming = (async () => {
          for (let revision = 1; revision <= 12; revision++) {
            const items = [...histories.get(a.id)];
            items[items.length - 1] = {
              ...items.at(-1),
              text: `Stream revision ${revision}. ${"Current response text. ".repeat(revision)}`,
            };
            await page.evaluate(
              ({ id, packet, revision }) =>
                window.emitFixture(id, packet, revision),
              {
                id: a.id,
                revision,
                packet: {
                  agent: agents[0],
                  items,
                  replace: true,
                  order: items.map((item) => item.id),
                  historyVersion: "fixed-history",
                },
              },
            );
            await page.waitForTimeout(100);
          }
        })();
        await Promise.all([
          streaming,
          page.keyboard.type(suffix, { delay: 35 }),
        ]);
        await page.waitForFunction(() => window.metrics.stream.length === 12);
        assert.equal(
          await input.inputValue(),
          firstText + suffix,
          "Streaming does not lose typed characters",
        );
        assert.equal(
          await page.locator('[data-message^="a-text-"]').count(),
          120,
          "Streaming retains all commentary",
        );
        assert.equal(
          await page.locator('[data-message^="a-result-"]').count(),
          30,
          "Streaming retains all final answers",
        );
        assert.equal(
          await page.locator('[data-message^="b-"]').count(),
          0,
          "The other chat does not leak into history",
        );
        assert.match(
          await page.locator('[data-message="a-live"]').innerText(),
          /Stream revision 12\./,
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
            ["idle-input", "scrolled-input", "stream-and-input"].map(
              (phase) => [
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
              ],
            ),
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
      await writeFile(
        join(dir, "report.json"),
        JSON.stringify(report, null, 2),
      );
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
  },
);
