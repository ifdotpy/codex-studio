import type {} from "./agent-phase-browser.fixture2.tsx";
import type { Snapshot } from "../../src/types.ts";
import { fixtureOrigin } from "../misc-fixture.ts";
import type {} from "./agent-phase-browser.fixture.tsx";
// Component check with an isolated Vite server. No backend or model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect, createFixtureViteServer } from "../playwright.ts";

test("chat reconnect follows transport across hidden conversations", async ({
  page,
}) => {
  test.setTimeout(60_000);
  const root = join(import.meta.dirname, "../../");
  const server = await createFixtureViteServer({
    configFile: false,
    root,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "chat-connection-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_request, response) => {
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<div id="root"></div><script type="module" src="/tests/agents/agent-phase-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  const requests: string[] = [];
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/**", (route) => {
    requests.push(route.request().url());
    return route.abort();
  });
  try {
    await server.listen();
    await page.goto(`${fixtureOrigin(server)}/check`);
    const first = page.locator('[data-chat="first"]');
    const second = page.locator('[data-chat="second"]');
    await expect(first).toHaveAttribute("data-loaded", "true");
    await expect(first).toHaveAttribute("data-connection", "live");
    await expect(first).toContainText("Writing");
    await expect(second).toHaveAttribute("data-connection", "");
    await page.evaluate(() =>
      window.chatConnectionFixture.setConnection("offline"),
    );
    await expect(first).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() =>
      window.chatConnectionFixture.setConnection("connecting"),
    );
    await expect(first).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() =>
      window.chatConnectionFixture.selectChat("second"),
    );
    await expect(second).toContainText("Connection lost. Reconnecting…");
    await expect(first).toHaveAttribute("data-connection", "");
    await page.evaluate(() => window.chatConnectionFixture.selectChat("first"));
    await expect(first).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() =>
      window.chatConnectionFixture.selectChat("second"),
    );
    await page.evaluate(() =>
      window.chatConnectionFixture.setConnection("connecting"),
    );
    await expect(second).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() => window.chatConnectionFixture.selectServer(true));
    await expect(second).toHaveAttribute("data-connection", "");
    await expect(second).not.toContainText("Connection lost.");
    await page.evaluate(() => window.chatConnectionFixture.selectServer(false));
    await expect(second).toContainText("Connection lost. Reconnecting…");
    await page.evaluate(() =>
      window.chatConnectionFixture.setConnection("live"),
    );
    await expect(second).toHaveAttribute("data-connection", "live");
    await expect(second).toContainText("Writing");
    await page.evaluate(() => window.chatConnectionFixture.selectChat("first"));
    await expect(first).toHaveAttribute("data-connection", "live");
    await expect(first).toContainText("Writing");
    await page.evaluate(() =>
      window.chatConnectionFixture.phase("first", "retrying"),
    );
    await expect(first).toContainText("Provider is reconnecting");
    await expect(first).not.toContainText("Connection lost.");
    await page.evaluate(() =>
      window.chatConnectionFixture.phase("first", "writing"),
    );
    await expect(first).toContainText("Writing");
    await expect(first).not.toContainText("Provider is reconnecting");
    expect(
      await page.evaluate(() =>
        window.chatConnectionFixture.connectionListeners(),
      ),
    ).toBe(2);
    const beforeRepeatedStatus = await page.evaluate(
      () => window.chatConnectionFixture.renders,
    );
    await page.evaluate(async () => {
      for (let repeated = 0; repeated < 20; repeated++)
        window.chatConnectionFixture.repeatLive();
      await new Promise((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(resolve)),
      );
    });
    expect(
      await page.evaluate(() => window.chatConnectionFixture.renders),
    ).toEqual(beforeRepeatedStatus);
    expect(
      await page.evaluate(() =>
        window.chatConnectionFixture.connectionListeners(),
      ),
    ).toBe(2);
    expect(
      await page.evaluate(() => window.chatConnectionFixture.reads),
    ).toEqual([]);
    expect(requests).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await server.close();
  }
});

test("agent phase browser", async ({ page: runnerPage }) => {
  const root = join(import.meta.dirname, "../../");
  const createServer = createFixtureViteServer;
  const evidence = await mkdtemp(join(tmpdir(), "studio-agent-phase-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir: join(evidence, "cache"),
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
      ],
    },
    plugins: [
      {
        name: "agent-phase-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/agents/agent-phase-browser.fixture2.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 1280, height: 900 });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(`${fixtureOrigin(server)}/check`);
    const phase = page.locator(".agent-phase");
    const summary = phase.locator("summary");
    await summary.getByText("Waiting for 2 agents", { exact: true }).waitFor();
    assert.equal(await phase.getAttribute("data-wait-state"), "live");
    const animation = () =>
      phase
        .locator(".phase-dots i")
        .evaluateAll((nodes) =>
          nodes.map((node) => getComputedStyle(node).animationName),
        );
    assert.deepEqual(await animation(), [
      "phase-pulse",
      "phase-pulse",
      "phase-pulse",
    ]);
    assert.equal(
      await page.locator("#indicator .chat-status").getAttribute("aria-label"),
      "Waiting for 2 agents",
    );
    await summary.click();
    await phase.getByText("Agent: Worker 1", { exact: true }).waitFor();
    await summary.click();
    const change = async (kind: string, count = 1) => {
      await page.evaluate(
        ({ kind, count }) => {
          const runtime: Snapshot["runtime"] = {
            studioErrors: [],
            agents: [],
            rooms: [],
            complaints: [],
            rules: [],
            projects: [],
            peerTeams: [],
            events: [],
            work: [],
            tasks: [],
            monitors: [],
            requests: [],
          };
          const lead = { ...window.agentPhaseFixture.base };
          let threads = [lead];
          if (kind === "agent")
            threads.push(
              ...Array.from({ length: count }, (_, i) =>
                window.agentPhaseFixture.worker(i),
              ),
            );
          if (kind === "command")
            runtime.tasks = Array.from({ length: count }, (_, i) =>
              window.agentPhaseFixture.command(i),
            );
          if (kind === "monitor")
            runtime.monitors = Array.from({ length: count }, (_, i) =>
              window.agentPhaseFixture.monitor(i),
            );
          if (kind === "event")
            Object.assign(lead, {
              status: "parked",
              parkedEvent: "build-ready",
              autoWake: false,
            });
          if (kind === "input")
            runtime.requests = Array.from({ length: count }, (_, i) => ({
              id: "input-" + i,
              agent: "lead",
              status: "pending",
              epoch: 2,
            }));
          if (kind === "mixed") {
            threads.push(
              window.agentPhaseFixture.worker(1),
              window.agentPhaseFixture.worker(2),
            );
            runtime.tasks = [window.agentPhaseFixture.command(1)];
            runtime.monitors = [window.agentPhaseFixture.monitor(1)];
            lead.parkedEvent = "very-long-event-name-".repeat(10);
            runtime.requests = [
              { id: "input", agent: "lead", status: "pending", epoch: 2 },
            ];
          }
          if (kind === "completed") lead.status = "completed";
          if (kind === "completed-monitor") {
            lead.status = "completed";
            runtime.monitors = [window.agentPhaseFixture.monitor(1)];
          }
          window.agentPhaseFixture.setData({ threads, runtime });
        },
        { kind, count },
      );
    };
    for (const kind of ["agent", "command", "monitor", "input"]) {
      await change(kind);
      assert.equal(await summary.textContent(), `Waiting for 1 ${kind}`);
      await change(kind, 2);
      assert.equal(await summary.textContent(), `Waiting for 2 ${kind}s`);
    }
    await change("event");
    assert.equal(await summary.textContent(), "Waiting for event build-ready");
    assert.equal(
      await page
        .locator("#indicator .chat-status")
        .getAttribute("data-chat-status"),
      "working",
    );
    await change("completed-monitor");
    assert.equal(await summary.textContent(), "Waiting for 1 monitor");
    for (const width of [1280, 390, 320]) {
      await page.setViewportSize({ width, height: 900 });
      await change("agent", 2);
      const box = await phase.boundingBox();
      const anchor = await page.locator("#anchor").boundingBox();
      await change("mixed");
      assert.equal((await phase.boundingBox())?.height, box?.height);
      assert.equal((await page.locator("#anchor").boundingBox())?.y, anchor?.y);
      assert(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      );
      await summary.click();
      await phase
        .getByText("Command: npm run check 1", { exact: true })
        .waitFor();
      await phase
        .getByText("Monitor: watch build 1", { exact: true })
        .waitFor();
      assert(
        await phase
          .locator(".agent-phase-wait-details")
          .evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
      );
      assert.equal(
        (await page.locator("#anchor").boundingBox())?.y,
        anchor?.y,
        "Open details cannot move the transcript",
      );
      await page.screenshot({ path: join(evidence, `waiting-${width}.png`) });
      await summary.click();
      await change("agent", 99);
      assert.equal((await phase.boundingBox())?.height, box?.height);
      assert.equal((await page.locator("#anchor").boundingBox())?.y, anchor?.y);
    }
    await page.emulateMedia({ reducedMotion: "reduce" });
    assert.deepEqual(await animation(), ["none", "none", "none"]);
    assert.equal(
      await page
        .locator("#indicator .chat-status-working svg")
        .evaluate((node) => getComputedStyle(node).animationName),
      "none",
    );
    await page.emulateMedia({ reducedMotion: "no-preference" });
    await change("none");
    assert.equal(await phase.getAttribute("data-wait-state"), "ended");
    assert.equal(
      await phase.innerText(),
      "Turn ended. Send a message to continue.",
    );
    assert.equal(await phase.locator(".phase-dots").count(), 0);
    assert.equal(await phase.getAttribute("class"), "agent-phase ");
    assert.equal(await page.locator("#indicator .chat-status").count(), 0);
    assert.deepEqual(
      await phase.evaluate((node) =>
        node
          .getAnimations({ subtree: true })
          .map((animation) =>
            animation instanceof CSSAnimation
              ? animation.animationName
              : undefined,
          ),
      ),
      [],
    );
    await page.screenshot({ path: join(evidence, "turn-ended-320.png") });
    await change("completed");
    assert.equal(await phase.count(), 0);
    expect(errors).toEqual([]);
    console.log(
      "PASS AgentPhase: exact wake counts and kinds, names by tap, live animation, reduced motion, static end, completed monitor, stable 1280/390/320px layouts",
    );
    console.log("Evidence:", evidence);
  } finally {
    await server.close();
  }
});
