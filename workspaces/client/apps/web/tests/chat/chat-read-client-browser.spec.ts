import { initialAgent, fixtureReadState } from "./chat-read-state.fixture.ts";
import type { EntityFixtureState } from "../playwright.ts";
import type { Connect } from "vite";
import { fixtureOrigin, trackBrowserContexts } from "./browser.ts";
import { handleEntitySyncFixtureRequest, test } from "../playwright.ts";
import assert from "node:assert/strict";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import type {} from "./chat-read-client-browser.fixture.tsx";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

trackBrowserContexts();

test("chat read client browser", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const root = join(import.meta.dirname, "../../");
  const workspaceId = "c".repeat(32);
  const syncAgent = { ...initialAgent };
  const syncSnapshot: EntityFixtureState = {
    stateDir: "/state",
    chats: [],
    edges: [],
    runtime: {
      agents: [syncAgent],
      projects: [],
      rooms: [],
      complaints: [],
      requests: [],
      monitors: [],
      tasks: [],
      events: [],
      peerTeams: [],
      rules: [],
      work: [],
      nativeNotices: [],
      sidebarOrder: { groups: null, revision: 0 },
      rateLimits: null,
      rateLimitsByAccount: {},
    },
  };
  let notifyState:
    | Parameters<
        NonNullable<
          Parameters<typeof handleEntitySyncFixtureRequest>[2]["onStreamReady"]
        >
      >[0]
    | undefined;
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-chat-read-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "dexie",
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
      ],
    },
    plugins: [
      {
        name: "chat-read-fixture",
        configureServer(server) {
          server.middlewares.stack.unshift({
            route: "",
            handle(
              req: Parameters<Connect.NextHandleFunction>[0],
              res: Parameters<Connect.NextHandleFunction>[1],
              next: Connect.NextFunction,
            ) {
              const url = new URL(req.url ?? "", "http://fixture.local");
              if (
                req.method === "POST" &&
                url.pathname === "/api/test/read-state"
              ) {
                let text = "";
                req.on("data", (chunk) => (text += chunk));
                req.on("end", () => {
                  syncAgent.readState = fixtureReadState(text);
                  notifyState?.([{ kind: "state" }]);
                  res.writeHead(204);
                  res.end();
                });
                return;
              }
              if (
                handleEntitySyncFixtureRequest(req, res, {
                  snapshot: syncSnapshot,
                  workspaceId,
                  onStreamReady(notify) {
                    notifyState = notify;
                  },
                })
              )
                return;
              next();
            },
          });
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<div id="root"></div><script type="module" src="/tests/chat/chat-read-client-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  let browser;
  try {
    await server.listen();
    browser = _browser;
    const page = await browser.newPage({
      viewport: { width: 390, height: 700 },
    });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const ready = async () =>
      page.waitForFunction(() => !!window.chatReadFixture.setAgent);
    const settle = () =>
      page.evaluate(
        () =>
          new Promise<void>((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
          ),
      );
    await page.clock.install();
    await page.goto(`${fixtureOrigin(server)}/check`);
    await ready();
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
    );
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.dispatchEvent(new Event("focus"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "Offscreen result is unread",
    );
    await page.evaluate(() => {
      window.chatReadFixture.visible = false;
      window.chatReadFixture.setOffset(0);
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "Hidden page is unread",
    );
    await page.evaluate(() => {
      window.chatReadFixture.visible = true;
      window.chatReadFixture.foreground = false;
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "Unfocused page is unread",
    );
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setLoaded(false);
      window.dispatchEvent(new Event("focus"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "Unloaded transcript is unread",
    );
    await page.evaluate(() => {
      window.chatReadFixture.setStreaming(true);
      window.chatReadFixture.setLoaded(true);
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "Streaming final is not yet a complete result",
    );
    await page.evaluate(() => {
      window.chatReadFixture.setPhase("commentary");
      window.chatReadFixture.setStreaming(false);
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "Visible commentary cannot acknowledge an absent final answer",
    );
    await page.evaluate(() => window.chatReadFixture.setPhase("final_answer"));
    await page.waitForFunction(
      () => window.chatReadFixture.state?.read === true,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      1,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls[0]?.workspace),
      "first",
    );
    await page.evaluate(() => {
      window.chatReadFixture.previousControls = window.chatReadFixture.controls;
      window.chatReadFixture.setAgent({
        ...window.chatReadFixture.agent,
        tail: "Old replica update",
      });
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.state?.read),
      true,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      1,
    );
    assert.equal(
      await page.evaluate(
        () =>
          window.chatReadFixture.previousControls?.observeRead ===
            window.chatReadFixture.controls.observeRead &&
          window.chatReadFixture.previousControls?.markUnread ===
            window.chatReadFixture.controls.markUnread &&
          window.chatReadFixture.previousControls?.readStateFor ===
            window.chatReadFixture.controls.readStateFor,
      ),
      true,
      "Unrelated updates preserve callback identities",
    );
    await page.evaluate(() => window.chatReadFixture.markUnread());
    await page.waitForFunction(
      () =>
        window.chatReadFixture.state?.read === false &&
        window.chatReadFixture.marking.length === 0,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      2,
    );
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      2,
    );
    await page.reload();
    await ready();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.state?.read),
      false,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
    );
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setOffset(0);
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForFunction(
      () => window.chatReadFixture.state?.read === true,
    );
    await page.evaluate(() => {
      const state = window.chatReadFixture.state;
      if (!state) throw new Error("Missing read state");
      window.chatReadFixture.setAgent({
        ...window.chatReadFixture.agent,
        readState: { ...state, read: false, revision: state.revision + 1 },
      });
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.state?.read),
      false,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      1,
      "Another device's Unread does not trigger auto-read on the unchanged visible result",
    );
    await page.evaluate(() => {
      const state = window.chatReadFixture.state;
      if (!state) throw new Error("Missing read state");
      window.chatReadFixture.setAgent({
        ...window.chatReadFixture.agent,
        readState: { ...state, read: true, revision: state.revision + 1 },
      });
    });
    await page.evaluate(() => {
      window.chatReadFixture.setAgent({
        ...window.chatReadFixture.agent,
        lastCompletedTurn: "new-turn",
      });
      window.chatReadFixture.observe({
        id: "one",
        threadId: "thread",
        turnId: "turn",
      });
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      1,
      "Old visible result cannot acknowledge newer result",
    );
    await page.evaluate(() => {
      window.chatReadFixture.setMessage("new-turn");
      window.chatReadFixture.hold = true;
      window.chatReadFixture.setOffset(0);
    });
    await page.waitForFunction(() => window.chatReadFixture.calls.length === 2);
    await page.evaluate(() => {
      window.chatReadFixture.foreground = false;
      window.chatReadFixture.setWorkspace("second");
    });
    await settle();
    const refreshes = await page.evaluate(
      () => window.chatReadFixture.refreshes,
    );
    const staleWrite = page.waitForResponse("**/api/test/read-state");
    await page.evaluate(() => {
      window.chatReadFixture.release();
      window.chatReadFixture.hold = false;
    });
    await (await staleWrite).finished();
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.refreshes),
      refreshes,
      "Old workspace response cannot refresh current scope",
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.state?.turnId),
      "turn",
      "Old workspace response cannot set canonical state",
    );
    await page.evaluate(() => {
      window.chatReadFixture.foreground = false;
      window.chatReadFixture.setAgent({
        ...window.chatReadFixture.agent,
        readStateSupported: false,
      });
      window.chatReadFixture.setWorkspace("third");
    });
    await settle();
    const count = await page.evaluate(
      () => window.chatReadFixture.calls.length,
    );
    await page.evaluate(() => window.chatReadFixture.markUnread());
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      count,
    );
    await page.evaluate(() => {
      window.chatReadFixture.setAgent({
        ...window.chatReadFixture.agent,
        readStateSupported: true,
      });
      window.chatReadFixture.setWorkspace("fourth");
      window.chatReadFixture.fail = true;
      window.chatReadFixture.foreground = true;
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForFunction(
      () => window.chatReadFixture.notifications.length > 0,
    );
    await settle();
    const failed = await page.evaluate(
      () => window.chatReadFixture.calls.length,
    );
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      failed,
      "A rejected acknowledgement cannot loop",
    );
    await page.evaluate(() => {
      window.chatReadFixture.foreground = false;
      window.chatReadFixture.fail = false;
      window.chatReadFixture.hold = true;
      window.chatReadFixture.setWorkspace("last");
      void window.chatReadFixture.markUnread();
    });
    await page.waitForFunction(() => window.chatReadFixture.marking.length > 0);
    const beforeUnmount = await page.evaluate(
      () => window.chatReadFixture.refreshes,
    );
    const unmountedWrite = page.waitForResponse("**/api/test/read-state");
    await page.evaluate(() => {
      window.chatReadFixture.unmount();
      window.chatReadFixture.release();
    });
    await (await unmountedWrite).finished();
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.refreshes),
      beforeUnmount,
    );
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.chatReadFixture.hold = true;
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setOffset(0);
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForFunction(() => window.chatReadFixture.calls.length === 1);
    await page.evaluate(() => {
      void window.chatReadFixture.markUnread();
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      1,
    );
    await page.evaluate(() => {
      window.chatReadFixture.hold = false;
      window.chatReadFixture.release();
    });
    await page.waitForFunction(
      () =>
        window.chatReadFixture.state?.read === false &&
        window.chatReadFixture.marking.length === 0,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      2,
    );
    assert.deepEqual(
      await page.evaluate(() =>
        window.chatReadFixture.calls.map((call) => call.read_state.read),
      ),
      [true, false],
    );
    // Earlier history can replace a keyed turn without changing the final message ID.
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await settle();
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.replaceResult();
    });
    await settle();
    await page.evaluate(() => window.chatReadFixture.setOffset(0));
    await page.waitForFunction(
      () => window.chatReadFixture.state?.read === true,
      undefined,
      {
        timeout: 2000,
      },
    );

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setPhase("commentary");
      window.chatReadFixture.setTurnStatus("completed");
      window.chatReadFixture.setLatestPage(false);
      window.chatReadFixture.setOffset(0);
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "Partial history cannot acknowledge an absent final answer",
    );
    await page.evaluate(() => window.chatReadFixture.setLatestPage(true));
    await page.waitForFunction(
      () => window.chatReadFixture.state?.read === true,
    );

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setPhase("commentary");
      window.chatReadFixture.setTurnStatus("completed");
      window.chatReadFixture.setLatestPage(true);
      window.chatReadFixture.setGroupTurn("historical-a");
      window.chatReadFixture.setGroupTurns("historical-a silent-b");
      window.chatReadFixture.setMessage("silent-b");
      window.chatReadFixture.setOffset(0);
      window.chatReadFixture.setNativeRows([
        ...Array.from({ length: 12 }, (_, index) => ({
          id: `history-a-${index}`,
          role: "tool",
          turnId: "historical-a",
          turnStatus: "completed",
          height: 80,
          text: `Historical activity ${index}`,
        })),
        {
          id: "activity-silent-b",
          role: "tool",
          turnId: "silent-b",
          turnStatus: "completed",
          height: 80,
          text: "Latest tool activity",
        },
      ]);
      window.chatReadFixture.setRowsOpen(false);
      window.chatReadFixture.setAgent({
        ...window.chatReadFixture.agent,
        lastCompletedTurn: "silent-b",
        lastCompletedTurnStatus: "completed",
      });
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "a collapsed joined disclosure does not acknowledge hidden latest-turn rows",
    );
    await page.evaluate(() => window.chatReadFixture.setRowsOpen(true));
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
      "visible historical rows cannot acknowledge latest-turn activity below the viewport",
    );
    assert.equal(
      await page.locator('[data-message="history-a-0"]').isVisible(),
      true,
      "historical A is visible at the top of the transcript",
    );
    assert.equal(
      await page.evaluate(() => {
        const rootNode = document.querySelector("#messages");
        if (!rootNode) throw new Error("Missing messages root");
        const root = rootNode.getBoundingClientRect();
        const latestNode = document.querySelector(
          '[data-message="activity-silent-b"]',
        );
        if (!latestNode) throw new Error("Missing latest message");
        const latest = latestNode.getBoundingClientRect();
        return (
          Math.min(root.bottom, latest.bottom) > Math.max(root.top, latest.top)
        );
      }),
      false,
      "latest B is below the transcript viewport",
    );
    await page.evaluate(() => {
      const root = document.querySelector("#messages");
      if (!root) throw new Error("Missing messages root");
      root.scrollTop = root.scrollHeight;
      root.dispatchEvent(new Event("scroll"));
    });
    await page.waitForFunction(() =>
      window.chatReadFixture.calls.some(
        (call) => call.read_state.turn_id === "silent-b",
      ),
    );
    assert.deepEqual(
      await page.evaluate(() =>
        window.chatReadFixture.calls.map((call) => call.read_state.turn_id),
      ),
      ["silent-b"],
      "a visible tool-only latest turn is acknowledged through joined historical membership",
    );

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      const fetch = window.fetch;
      window.chatReadFixture.transientFailure = true;
      window.fetch = (input, options) => {
        const request =
          input instanceof Request ? input : new Request(input, options);
        if (
          new URL(request.url).pathname === "/api/organization" &&
          window.chatReadFixture.transientFailure
        ) {
          window.chatReadFixture.transientFailure = false;
          return Promise.reject(new TypeError("Network disconnected"));
        }
        return fetch(input, options);
      };
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setOffset(0);
    });
    await page.waitForFunction(() => !window.chatReadFixture.transientFailure);
    await page.clock.fastForward(1000);
    await page.waitForFunction(
      () => window.chatReadFixture.state?.read === true,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      1,
    );

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setResultText("");
      window.chatReadFixture.setTurnStatus("completed");
      window.chatReadFixture.setOffset(0);
    });
    await page.waitForFunction(
      () => window.chatReadFixture.state?.read === true,
    );

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setPhase("commentary");
      window.chatReadFixture.setTurnStatus("completed");
      const turn = document.querySelector<HTMLElement>("[data-turn]");
      if (!turn) throw new Error("Missing turn");
      turn.dataset.outcome = "active";
      window.chatReadFixture.setOffset(0);
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.calls.length),
      0,
    );
    await page.evaluate(() => {
      const turn = document.querySelector<HTMLElement>("[data-turn]");
      if (!turn) throw new Error("Missing turn");
      turn.dataset.outcome = "completed";
    });
    await page.waitForFunction(
      () => window.chatReadFixture.state?.read === true,
    );

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      const fetch = window.fetch;
      window.chatReadFixture.lostWrites = [];
      window.chatReadFixture.lostReplies = 0;
      window.chatReadFixture.releaseLost = null;
      window.fetch = async (input, options) => {
        const request =
          input instanceof Request ? input : new Request(input, options);
        if (new URL(request.url).pathname !== "/api/organization")
          return fetch(input, options);
        const body = window.chatReadFixture.readWrite(
          JSON.parse(await request.clone().text()),
        );
        if (!body.read_state.read) return fetch(input, options);
        window.chatReadFixture.lostWrites.push(body);
        // The first request commits. All three replies are lost.
        if (window.chatReadFixture.lostWrites.length === 1)
          await fetch(input, options);
        if (window.chatReadFixture.lostWrites.length === 3)
          await new Promise<void>(
            (resolve) => (window.chatReadFixture.releaseLost = resolve),
          );
        window.chatReadFixture.lostReplies++;
        throw new TypeError("Response lost");
      };
      window.chatReadFixture.foreground = true;
      window.chatReadFixture.setOffset(0);
    });
    for (const [replies, backoff] of [
      [1, 1000],
      [2, 2000],
    ] as const) {
      await page.waitForFunction(
        (count) => window.chatReadFixture.lostReplies === count,
        replies,
      );
      await page.clock.fastForward(backoff);
    }
    await page.waitForFunction(() => !!window.chatReadFixture.releaseLost);
    await page.evaluate(() => {
      void window.chatReadFixture.markUnread();
      const release = window.chatReadFixture.releaseLost;
      if (!release) throw new Error("Missing held reply");
      release();
    });
    await page.waitForFunction(
      () =>
        window.chatReadFixture.marking.length === 0 &&
        window.chatReadFixture.state?.read === false,
    );
    assert.equal(
      await page.evaluate(() => window.chatReadFixture.serverState?.read),
      false,
    );
    assert.deepEqual(
      await page.evaluate(() =>
        window.chatReadFixture.lostWrites.map(
          (v) => v.read_state.expected_revision,
        ),
      ),
      [0, 0, 0],
    );
    assert.deepEqual(
      await page.evaluate(() =>
        window.chatReadFixture.calls.map((v) => v.read_state.read),
      ),
      [true, false],
    );
    assert.deepEqual(errors, []);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
