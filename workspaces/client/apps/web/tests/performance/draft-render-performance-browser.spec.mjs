// Real draft hook, local durability, and consumer commits with 500 saved chats.
import { fileURLToPath } from "node:url";
import { apiSchemaHandshakeSse, test, expect } from "../playwright.mjs";

test("Draft Render Performance Browser @performance", async ({
  browser: testBrowser,
  context: _testContext,
  page: _testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const { createServer } = await import(
    root + "/workspaces/client/apps/web/node_modules/vite/dist/node/index.js"
  );
  const server = await createServer({
    configFile: false,
    root: root + "/workspaces/client/apps/web",
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  const browser = testBrowser;
  let context;
  try {
    for (const count of [500]) {
      context = await browser.newContext();
      const page = await context.newPage();
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      const workspaceId = "d".repeat(32);
      await page.route("**/check", (r) =>
        r.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Isolated draft hook audit</title>",
        }),
      );
      await page.route("**/api/sync/identity", (r) =>
        r.fulfill({ json: { workspaceId } }),
      );
      await page.route("**/api/sync/drafts", (r) => r.fulfill({ json: [] }));
      await page.route("**/api/sync/pull?*", (r) =>
        r.fulfill({
          json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
        }),
      );
      await page.route("**/api/sync/stream*", (r) =>
        r.fulfill({
          contentType: "text/event-stream",
          body: apiSchemaHandshakeSse(),
        }),
      );
      await page.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/check`,
      );
      // The first development request starts Vite's dependency optimizer. Do
      // not import app modules until that one-time scan/build has settled.
      await page.waitForTimeout(5000);
      await page.evaluate(
        async ({ count, workspaceId }) => {
          const importWithRetry = async (url) => {
            for (let attempt = 0; ; attempt++) {
              try {
                return await import(url);
              } catch (error) {
                if (
                  attempt === 11 ||
                  !String(error).includes(
                    "Failed to fetch dynamically imported module",
                  )
                )
                  throw error;
                await new Promise((resolve) => setTimeout(resolve, 250));
              }
            }
          };
          localStorage.setItem(
            "codex-sync-workspace",
            JSON.stringify(workspaceId),
          );
          const drafts = {};
          for (let i = 0; i < count; i++) {
            const session = "chat-" + i;
            const text = "x".repeat(2000);
            drafts[session] = text;
            localStorage.setItem(
              `codex-chat-draft:${workspaceId}:${session}`,
              JSON.stringify({
                version: 1,
                workspace: workspaceId,
                session,
                text,
                deleted: false,
                source: "local",
                updated: 1,
              }),
            );
          }
          const { useSyncedDrafts } = await importWithRetry(
            "/src/sync/drafts.ts",
          );
          try {
            window.PromptComposer = (
              await importWithRetry(
                "/src/components/prompt-composer/PromptComposer.tsx",
              )
            ).default;
          } catch {
            window.PromptComposer = null;
          }
          const { db } = await (
            await importWithRetry("/src/sync/client.ts")
          ).syncDatabase();
          window.db = db;
          const docs = Object.entries(drafts).map(([session, text]) => ({
            id: "fixture:" + session,
            seq: 0,
            payload: JSON.stringify({
              id: "fixture:" + session,
              device: "fixture",
              session,
              text,
              updated: 1,
            }),
          }));
          await db.drafts.bulkInsert(docs);
          const r = await importWithRetry("/node_modules/.vite/deps/react.js"),
            d = await importWithRetry(
              "/node_modules/.vite/deps/react-dom_client.js",
            );
          const React = r.default || r,
            { createRoot } = d.default || d;
          const node = document.createElement("div");
          document.body.append(node);
          window.commits = 0;
          window.renders = 0;
          window.composerCommits = 0;
          window.sidebarRenders = 0;
          window.transcriptRenders = 0;
          createRoot(node).render(
            React.createElement(
              React.Profiler,
              { id: "draft", onRender: () => window.commits++ },
              React.createElement(function Harness() {
                window.renders++;
                window.draft = useSyncedDrafts();
                return React.createElement(
                  React.Fragment,
                  null,
                  React.createElement(function SidebarFixture() {
                    window.sidebarRenders++;
                    return React.createElement("aside");
                  }),
                  React.createElement(function TranscriptFixture() {
                    window.transcriptRenders++;
                    return React.createElement("main");
                  }),
                  typeof window.draft.getDraft === "function" &&
                    window.PromptComposer
                    ? React.createElement(
                        window.PromptComposer,
                        {
                          session: "chat-0",
                          getDraft: window.draft.getDraft,
                          subscribeDraft: window.draft.subscribeDraft,
                          onSubmit: (event) => event.preventDefault(),
                        },
                        (value) =>
                          React.createElement(
                            React.Profiler,
                            {
                              id: "prompt-composer",
                              onRender: () => window.composerCommits++,
                            },
                            React.createElement("span", null, value),
                          ),
                      )
                    : React.createElement(
                        React.Profiler,
                        {
                          id: "prompt-composer",
                          onRender: () => window.composerCommits++,
                        },
                        React.createElement(
                          "span",
                          null,
                          window.draft.drafts["chat-0"],
                        ),
                      ),
                );
              }),
            ),
          );
        },
        { count, workspaceId },
      );
      await page.waitForFunction(
        () => window.draft?.drafts["chat-0"] === "x".repeat(2000),
      );
      await page.waitForTimeout(1200);
      if (browser.browserType().name() === "chromium") {
        const cdp = await context.newCDPSession(page);
        await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
      }
      const result = await page.evaluate(
        async ({ workspaceId }) => {
          const baseCommits = window.commits,
            baseRenders = window.renders,
            baseComposerCommits = window.composerCommits,
            baseSidebarRenders = window.sidebarRenders,
            baseTranscriptRenders = window.transcriptRenders;
          let chatWrites = 0,
            chatBytes = 0,
            journalWrites = 0;
          const original = Storage.prototype.setItem;
          Storage.prototype.setItem = function (k, v) {
            if (k.startsWith(`codex-drafts:${workspaceId}:pending:`))
              journalWrites++;
            else if (k.startsWith("codex-chat-draft:")) {
              chatWrites++;
              chatBytes += v.length * 2;
            }
            return original.call(this, k, v);
          };
          const setTimes = [];
          for (let i = 0; i < 20; i++) {
            const at = performance.now();
            window.draft.setDrafts((old) => ({
              ...old,
              "chat-0": "typed " + i,
            }));
            const local = localStorage.getItem(
              "codex-chat-draft:" + workspaceId + ":chat-0",
            );
            const persisted = JSON.parse(local).text;
            if (persisted !== "typed " + i)
              throw new Error(
                "Each edit must synchronously reach its local chat record",
              );
            if (
              !Object.keys(localStorage).some(
                (k) =>
                  k.includes(":pending:") &&
                  JSON.parse(localStorage.getItem(k)).text === "typed " + i,
              )
            )
              throw new Error("Each edit must synchronously reach the journal");
            setTimes.push(performance.now() - at);
            await new Promise((r) => setTimeout(r, 40));
          }
          await new Promise((r) => setTimeout(r, 500));
          Storage.prototype.setItem = original;
          const sorted = setTimes.slice().sort((a, b) => a - b);
          const migrationChatBytes = Object.keys(localStorage)
            .filter((key) => key.startsWith("codex-chat-draft:"))
            .reduce(
              (bytes, key) => bytes + localStorage.getItem(key).length * 2,
              0,
            );
          return {
            setMedianMs: sorted[10],
            setMaxMs: Math.max(...setTimes),
            commits: window.commits - baseCommits,
            renders: window.renders - baseRenders,
            composerCommits: window.composerCommits - baseComposerCommits,
            sidebarRenders: window.sidebarRenders - baseSidebarRenders,
            transcriptRenders: window.transcriptRenders - baseTranscriptRenders,
            chatWrites,
            chatBytes,
            migrationChatBytes,
            journalWrites,
          };
        },
        { workspaceId },
      );
      assert.equal(
        result.chatWrites,
        20,
        "Each edit writes only its per-chat record",
      );
      assert.equal(
        result.journalWrites,
        20,
        "Each input keeps its synchronous journal write",
      );
      assert.equal(
        result.commits,
        20,
        "The active composer must still commit once per edit",
      );
      // The pinned 2470fb5 baseline already keeps draft subscriptions isolated
      // from the owning hook; compare storage/latency without attributing an
      // intervening React-render refactor to this migration.
      const expectedOwnerRenders = 0;
      assert.equal(
        result.renders,
        expectedOwnerRenders,
        `Expected ${expectedOwnerRenders} harness renders for current draft storage`,
      );
      assert.equal(
        result.sidebarRenders,
        expectedOwnerRenders,
        "Harness sidebar does not rerender for a composer edit",
      );
      assert.equal(
        result.transcriptRenders,
        expectedOwnerRenders,
        "Harness transcript does not rerender for a composer edit",
      );
      assert.equal(
        result.composerCommits,
        20,
        "Only the active composer should commit for the 20 edits",
      );
      assert.ok(result.chatBytes < 400_000, "Per-chat edits stay below 400 KB");
      const noOp = await page.evaluate(async () => {
        const before = window.commits;
        const original = Storage.prototype.setItem;
        let writes = 0;
        Storage.prototype.setItem = function (k, v) {
          if (
            k.startsWith("codex-drafts:") ||
            k.startsWith("codex-chat-draft:")
          )
            writes++;
          return original.call(this, k, v);
        };
        const doc = await db.drafts.findOne("fixture:chat-10").exec();
        await doc.incrementalPatch({ seq: 100 });
        await new Promise((r) => setTimeout(r, 300));
        Storage.prototype.setItem = original;
        return { commits: window.commits - before, writes };
      });
      assert.deepEqual(
        noOp,
        { commits: 0, writes: 0 },
        "Unchanged remote text preserves consumer state and local storage",
      );
      await page.evaluate(async () => {
        await db.drafts.incrementalUpsert({
          id: "remote:chat-0",
          seq: 200,
          payload: JSON.stringify({
            id: "remote:chat-0",
            session: "chat-0",
            device: "remote",
            updated: Date.now() + 1000,
            text: "Concurrent remote text",
          }),
        });
      });
      // Current behavior keeps the local edit and presents the remote branch
      // through DraftVersions. Conversation's Replace text action calls
      // setDraft with the selected version, so exercise that public hook path.
      await page.waitForFunction(
        () =>
          draft.drafts["chat-0"] === "typed 19" &&
          draft.conflicts.some(
            (version) => version.text === "Concurrent remote text",
          ),
      );
      const remote = await page.evaluate(
        () =>
          draft.conflicts.find(
            (version) => version.text === "Concurrent remote text",
          ).text,
      );
      await page.evaluate((text) => {
        draft.setDrafts((current) => ({ ...current, "chat-0": text }));
      }, remote);
      await page.waitForFunction(
        () => draft.drafts["chat-0"] === "Concurrent remote text",
      );
      await page.waitForFunction(
        () =>
          draft.drafts["chat-0"] === "Concurrent remote text" &&
          !draft.conflicts.some((v) => v.text === "typed 19"),
      );
      assert.equal(
        await page.evaluate(() => draft.drafts["chat-499"]),
        "x".repeat(2000),
        "Unrelated drafts remain exact",
      );
      const other = await context.newPage();
      other.on("pageerror", (error) => errors.push(error.message));
      await other.route("**/check", (route) =>
        route.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Second draft tab</title>",
        }),
      );
      await other.route("**/api/sync/identity", (route) =>
        route.fulfill({ json: { workspaceId } }),
      );
      await other.route("**/api/sync/drafts", (route) =>
        route.fulfill({ json: [] }),
      );
      await other.route("**/api/sync/pull?*", (route) =>
        route.fulfill({
          json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
        }),
      );
      await other.route("**/api/sync/stream*", (route) =>
        route.fulfill({
          contentType: "text/event-stream",
          body: apiSchemaHandshakeSse(),
        }),
      );
      await other.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/check`,
      );
      await other.evaluate(async () => {
        const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
        const r = await import("/node_modules/.vite/deps/react.js");
        const d = await import("/node_modules/.vite/deps/react-dom_client.js");
        const React = r.default || r;
        const { createRoot } = d.default || d;
        const node = document.createElement("div");
        document.body.appendChild(node);
        createRoot(node).render(
          React.createElement(function Harness() {
            window.draft = useSyncedDrafts();
            return null;
          }),
        );
      });
      await other.waitForFunction(
        () => window.draft?.drafts["chat-0"] === "Concurrent remote text",
      );
      await other.evaluate(() =>
        draft.setDrafts((old) => ({
          ...old,
          "chat-1": "Exact second tab edit",
        })),
      );
      await page.waitForFunction(
        () => draft.drafts["chat-1"] === "Exact second tab edit",
      );
      assert.equal(
        await page.evaluate(() => draft.drafts["chat-0"]),
        "Concurrent remote text",
        "A second tab edit does not replace another chat",
      );
      assert.equal(
        await other.evaluate(() =>
          draft.conflicts.some((version) => version.text === "typed 19"),
        ),
        false,
        "The replaced local alternative does not return in a new tab",
      );
      await other.close();
      assert.deepEqual(
        errors,
        [],
        "Both draft consumers have no runtime errors",
      );
      console.log(
        JSON.stringify({
          browser: browser.browserType().name(),
          count,
          edits: 20,
          ...result,
          noOp,
        }),
      );
      await context.close();
      context = undefined;
    }
  } finally {
    await context?.close();
    await server.close();
  }
});
