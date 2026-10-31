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
  type ReadSnapshot = Omit<EntityFixtureState, "runtime"> & {
    runtime: Omit<
      NonNullable<EntityFixtureState["runtime"]>,
      "sidebarOrder" | "rateLimits"
    >;
  };
  const syncSnapshot = {
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
      sidebarOrder: [],
      rateLimits: [],
      rateLimitsByAccount: {},
    },
  };
  // The fixture retains legacy arrays for sidebarOrder and rateLimits.
  const readSnapshot: ReadSnapshot = syncSnapshot;
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
              const url = new URL(req.url!, "http://fixture.local");
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
                  snapshot: readSnapshot as EntityFixtureState,
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
    const ready = async () => page.waitForFunction(() => !!window.setAgent);
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
    assert.equal(await page.evaluate(() => window.calls.length), 0);
    await page.evaluate(() => {
      window.foreground = true;
      window.dispatchEvent(new Event("focus"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "Offscreen result is unread",
    );
    await page.evaluate(() => {
      window.visible = false;
      window.setOffset(0);
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "Hidden page is unread",
    );
    await page.evaluate(() => {
      window.visible = true;
      window.foreground = false;
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "Unfocused page is unread",
    );
    await page.evaluate(() => {
      window.foreground = true;
      window.setLoaded(false);
      window.dispatchEvent(new Event("focus"));
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "Unloaded transcript is unread",
    );
    await page.evaluate(() => {
      window.setStreaming(true);
      window.setLoaded(true);
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "Streaming final is not yet a complete result",
    );
    await page.evaluate(() => {
      window.setPhase("commentary");
      window.setStreaming(false);
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "Visible commentary cannot acknowledge an absent final answer",
    );
    await page.evaluate(() => window.setPhase("final_answer"));
    await page.waitForFunction(() => window.state?.read === true);
    assert.equal(await page.evaluate(() => window.calls.length), 1);
    assert.equal(
      await page.evaluate(() => window.calls[0]?.workspace),
      "first",
    );
    await page.evaluate(() => {
      window.previousControls = window.controls;
      window.setAgent({
        ...window.agent,
        tail: "Old replica update",
      });
    });
    await settle();
    assert.equal(await page.evaluate(() => window.state!.read), true);
    assert.equal(await page.evaluate(() => window.calls.length), 1);
    assert.equal(
      await page.evaluate(
        () =>
          window.previousControls!.observeRead ===
            window.controls.observeRead &&
          window.previousControls!.markUnread === window.controls.markUnread &&
          window.previousControls!.readStateFor ===
            window.controls.readStateFor,
      ),
      true,
      "Unrelated updates preserve callback identities",
    );
    await page.evaluate(() => window.markUnread());
    await page.waitForFunction(
      () => window.state?.read === false && window.marking.length === 0,
    );
    assert.equal(await page.evaluate(() => window.calls.length), 2);
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    await settle();
    assert.equal(await page.evaluate(() => window.calls.length), 2);
    await page.reload();
    await ready();
    assert.equal(await page.evaluate(() => window.state!.read), false);
    assert.equal(await page.evaluate(() => window.calls.length), 0);
    await page.evaluate(() => {
      window.foreground = true;
      window.setOffset(0);
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForFunction(() => window.state?.read === true);
    await page.evaluate(() => {
      const state = window.state!;
      window.setAgent({
        ...window.agent,
        readState: { ...state, read: false, revision: state.revision + 1 },
      });
    });
    await settle();
    assert.equal(await page.evaluate(() => window.state!.read), false);
    assert.equal(
      await page.evaluate(() => window.calls.length),
      1,
      "Another device's Unread does not trigger auto-read on the unchanged visible result",
    );
    await page.evaluate(() => {
      const state = window.state!;
      window.setAgent({
        ...window.agent,
        readState: { ...state, read: true, revision: state.revision + 1 },
      });
    });
    await page.evaluate(() => {
      window.setAgent({
        ...window.agent,
        lastCompletedTurn: "new-turn",
      });
      window.observe({
        id: "one",
        threadId: "thread",
        turnId: "turn",
      });
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      1,
      "Old visible result cannot acknowledge newer result",
    );
    await page.evaluate(() => {
      window.setMessage("new-turn");
      window.hold = true;
      window.setOffset(0);
    });
    await page.waitForFunction(() => window.calls.length === 2);
    await page.evaluate(() => {
      window.foreground = false;
      window.setWorkspace("second");
    });
    await settle();
    const refreshes = await page.evaluate(() => window.refreshes);
    const staleWrite = page.waitForResponse("**/api/test/read-state");
    await page.evaluate(() => {
      window.release();
      window.hold = false;
    });
    await (await staleWrite).finished();
    await settle();
    assert.equal(
      await page.evaluate(() => window.refreshes),
      refreshes,
      "Old workspace response cannot refresh current scope",
    );
    assert.equal(
      await page.evaluate(() => window.state?.turnId),
      "turn",
      "Old workspace response cannot set canonical state",
    );
    await page.evaluate(() => {
      window.foreground = false;
      window.setAgent({
        ...window.agent,
        readStateSupported: false,
      });
      window.setWorkspace("third");
    });
    await settle();
    const count = await page.evaluate(() => window.calls.length);
    await page.evaluate(() => window.markUnread());
    await settle();
    assert.equal(await page.evaluate(() => window.calls.length), count);
    await page.evaluate(() => {
      window.setAgent({
        ...window.agent,
        readStateSupported: true,
      });
      window.setWorkspace("fourth");
      window.fail = true;
      window.foreground = true;
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForFunction(() => window.notifications.length > 0);
    await settle();
    const failed = await page.evaluate(() => window.calls.length);
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      failed,
      "A rejected acknowledgement cannot loop",
    );
    await page.evaluate(() => {
      window.foreground = false;
      window.fail = false;
      window.hold = true;
      window.setWorkspace("last");
      void window.markUnread();
    });
    await page.waitForFunction(() => window.marking.length > 0);
    const beforeUnmount = await page.evaluate(() => window.refreshes);
    const unmountedWrite = page.waitForResponse("**/api/test/read-state");
    await page.evaluate(() => {
      window.unmount();
      window.release();
    });
    await (await unmountedWrite).finished();
    await settle();
    assert.equal(await page.evaluate(() => window.refreshes), beforeUnmount);
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.hold = true;
      window.foreground = true;
      window.setOffset(0);
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForFunction(() => window.calls.length === 1);
    await page.evaluate(() => {
      void window.markUnread();
    });
    await settle();
    assert.equal(await page.evaluate(() => window.calls.length), 1);
    await page.evaluate(() => {
      window.hold = false;
      window.release();
    });
    await page.waitForFunction(
      () => window.state?.read === false && window.marking.length === 0,
    );
    assert.equal(await page.evaluate(() => window.calls.length), 2);
    assert.deepEqual(
      await page.evaluate(() =>
        window.calls.map((call) => call.read_state.read),
      ),
      [true, false],
    );
    // Earlier history can replace a keyed turn without changing the final message ID.
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await settle();
    await page.evaluate(() => {
      window.foreground = true;
      window.replaceResult();
    });
    await settle();
    await page.evaluate(() => window.setOffset(0));
    await page.waitForFunction(() => window.state?.read === true, undefined, {
      timeout: 2000,
    });

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.foreground = true;
      window.setPhase("commentary");
      window.setTurnStatus("completed");
      window.setLatestPage(false);
      window.setOffset(0);
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "Partial history cannot acknowledge an absent final answer",
    );
    await page.evaluate(() => window.setLatestPage(true));
    await page.waitForFunction(() => window.state?.read === true);

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.foreground = true;
      window.setPhase("commentary");
      window.setTurnStatus("completed");
      window.setLatestPage(true);
      window.setGroupTurn("historical-a");
      window.setGroupTurns("historical-a silent-b");
      window.setMessage("silent-b");
      window.setOffset(0);
      window.setNativeRows([
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
      window.setRowsOpen(false);
      window.setAgent({
        ...window.agent,
        lastCompletedTurn: "silent-b",
        lastCompletedTurnStatus: "completed",
      });
    });
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
      0,
      "a collapsed joined disclosure does not acknowledge hidden latest-turn rows",
    );
    await page.evaluate(() => window.setRowsOpen(true));
    await settle();
    assert.equal(
      await page.evaluate(() => window.calls.length),
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
        const rootNode = document.querySelector("#messages")!;
        const root = rootNode.getBoundingClientRect();
        const latestNode = document.querySelector(
          '[data-message="activity-silent-b"]',
        )!;
        const latest = latestNode.getBoundingClientRect();
        return (
          Math.min(root.bottom, latest.bottom) > Math.max(root.top, latest.top)
        );
      }),
      false,
      "latest B is below the transcript viewport",
    );
    await page.evaluate(() => {
      const root = document.querySelector("#messages")!;
      root.scrollTop = root.scrollHeight;
      root.dispatchEvent(new Event("scroll"));
    });
    await page.waitForFunction(() =>
      window.calls.some((call) => call.read_state.turn_id === "silent-b"),
    );
    assert.deepEqual(
      await page.evaluate(() =>
        window.calls.map((call) => call.read_state.turn_id),
      ),
      ["silent-b"],
      "a visible tool-only latest turn is acknowledged through joined historical membership",
    );

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      const fetch = window.fetch;
      window.transientFailure = true;
      window.fetch = (input, options) => {
        const request =
          input instanceof Request ? input : new Request(input, options);
        if (
          new URL(request.url).pathname === "/api/organization" &&
          window.transientFailure
        ) {
          window.transientFailure = false;
          return Promise.reject(new TypeError("Network disconnected"));
        }
        return fetch(input, options);
      };
      window.foreground = true;
      window.setOffset(0);
    });
    await page.waitForFunction(() => !window.transientFailure);
    await page.clock.fastForward(1000);
    await page.waitForFunction(() => window.state?.read === true);
    assert.equal(await page.evaluate(() => window.calls.length), 1);

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.foreground = true;
      window.setResultText("");
      window.setTurnStatus("completed");
      window.setOffset(0);
    });
    await page.waitForFunction(() => window.state?.read === true);

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      window.foreground = true;
      window.setPhase("commentary");
      window.setTurnStatus("completed");
      const turn = document.querySelector<HTMLElement>("[data-turn]")!;
      turn.dataset.outcome = "active";
      window.setOffset(0);
    });
    await settle();
    assert.equal(await page.evaluate(() => window.calls.length), 0);
    await page.evaluate(() => {
      const turn = document.querySelector<HTMLElement>("[data-turn]")!;
      turn.dataset.outcome = "completed";
    });
    await page.waitForFunction(() => window.state?.read === true);

    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await ready();
    await page.evaluate(() => {
      const fetch = window.fetch;
      window.lostWrites = [];
      window.lostReplies = 0;
      window.releaseLost = null;
      window.fetch = async (input, options) => {
        const request =
          input instanceof Request ? input : new Request(input, options);
        if (new URL(request.url).pathname !== "/api/organization")
          return fetch(input, options);
        const body = window.readWrite(JSON.parse(await request.clone().text()));
        if (!body.read_state.read) return fetch(input, options);
        window.lostWrites.push(body);
        // The first request commits. All three replies are lost.
        if (window.lostWrites.length === 1) await fetch(input, options);
        if (window.lostWrites.length === 3)
          await new Promise<void>((resolve) => (window.releaseLost = resolve));
        window.lostReplies++;
        throw new TypeError("Response lost");
      };
      window.foreground = true;
      window.setOffset(0);
    });
    for (const [replies, backoff] of [
      [1, 1000],
      [2, 2000],
    ] as const) {
      await page.waitForFunction(
        (count) => window.lostReplies === count,
        replies,
      );
      await page.clock.fastForward(backoff);
    }
    await page.waitForFunction(() => !!window.releaseLost);
    await page.evaluate(() => {
      void window.markUnread();
      const release = window.releaseLost!;
      release();
    });
    await page.waitForFunction(
      () => window.marking.length === 0 && window.state?.read === false,
    );
    assert.equal(await page.evaluate(() => window.serverState!.read), false);
    assert.deepEqual(
      await page.evaluate(() =>
        window.lostWrites.map((v) => v.read_state.expected_revision),
      ),
      [0, 0, 0],
    );
    assert.deepEqual(
      await page.evaluate(() => window.calls.map((v) => v.read_state.read)),
      [true, false],
    );
    assert.deepEqual(errors, []);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
