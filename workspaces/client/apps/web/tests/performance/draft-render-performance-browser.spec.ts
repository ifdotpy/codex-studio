import type { Page } from "@playwright/test";
import type { components } from "../../src/generated/api.ts";
import type { LocalDraftRecord } from "../../src/sync/draftStorage.ts";
import { fixtureOrigin } from "./performance-fixture.ts";
import assert from "node:assert/strict";
import type {} from "./draft-render-performance.fixture.tsx";
// Real draft hook, local durability, and consumer commits with 500 saved chats.
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  readApiSchemaHash,
  test,
} from "../playwright.ts";

test("Draft Render Performance Browser @performance", async ({
  browser: testBrowser,
  context: _testContext,
  page: _testPage,
}) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const { createServer } = await import("vite");
  const server = await createServer({
    configFile: false,
    root: root + "/workspaces/client/apps/web",
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "draft-fixture",
        resolveId(id) {
          if (id === "/draft-render-performance.fixture.tsx")
            return fileURLToPath(
              new URL(
                "./draft-render-performance.fixture.tsx",
                import.meta.url,
              ),
            );
        },
      },
    ],
  });
  await server.listen();
  const browser = testBrowser;
  let context;
  try {
    for (const count of [500]) {
      context = await browser.newContext();
      const page = await context.newPage();
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));
      const workspaceId = "d".repeat(32);
      await page.route("**/check", (r) =>
        r.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Isolated draft hook audit</title>",
        }),
      );
      await installDraftSyncRoutes(page, workspaceId);

      await page.goto(`${fixtureOrigin(server)}/check`);
      // The first development request starts Vite's dependency optimizer. Do
      // not import app modules until that one-time scan/build has settled.
      await page.waitForTimeout(5000);
      await page.evaluate(
        async (options) =>
          (
            await import("./draft-render-performance.fixture.tsx")
          ).mountDraftFixture(options),
        { count, workspaceId },
      );
      await page.waitForFunction(
        () =>
          window.draftPerformanceDraft?.drafts["chat-0"] === "x".repeat(2000),
      );
      await page.waitForTimeout(1200);
      if (browser.browserType().name() === "chromium") {
        const cdp = await context.newCDPSession(page);
        await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
      }
      const result = await page.evaluate(
        async ({ workspaceId }) => {
          // These records are written by this fixture and the draft store under test.
          const readPersistedText = (raw: string | null) => {
            const record: Pick<LocalDraftRecord, "text"> = JSON.parse(
              raw ?? "null",
            );
            return record.text;
          };
          const baseCommits = window.draftPerformanceCommits,
            baseRenders = window.draftPerformanceRenders,
            baseComposerCommits = window.draftPerformanceComposerCommits,
            baseSidebarRenders = window.draftPerformanceSidebarRenders,
            baseTranscriptRenders = window.draftPerformanceTranscriptRenders;
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
            window.draftPerformanceDraft.setDrafts((old) => ({
              ...old,
              "chat-0": "typed " + i,
            }));
            const local = localStorage.getItem(
              "codex-chat-draft:" + workspaceId + ":chat-0",
            );
            const persisted = readPersistedText(local);
            if (persisted !== "typed " + i)
              throw new Error(
                "Each edit must synchronously reach its local chat record",
              );
            if (
              !Object.keys(localStorage).some(
                (k) =>
                  k.includes(":pending:") &&
                  readPersistedText(localStorage.getItem(k)) === "typed " + i,
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
              (bytes, key) =>
                bytes + (localStorage.getItem(key)?.length ?? 0) * 2,
              0,
            );
          return {
            setMedianMs: sorted[10],
            setMaxMs: Math.max(...setTimes),
            commits: window.draftPerformanceCommits - baseCommits,
            renders: window.draftPerformanceRenders - baseRenders,
            composerCommits:
              window.draftPerformanceComposerCommits - baseComposerCommits,
            sidebarRenders:
              window.draftPerformanceSidebarRenders - baseSidebarRenders,
            transcriptRenders:
              window.draftPerformanceTranscriptRenders - baseTranscriptRenders,
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
        const before = window.draftPerformanceCommits;
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
        const doc = await window.draftPerformanceDb.drafts
          .findOne("fixture:chat-10")
          .exec();
        if (!doc) throw new Error("Missing draft fixture document");
        await doc.incrementalPatch({ seq: 100 });
        await new Promise((r) => setTimeout(r, 300));
        Storage.prototype.setItem = original;
        return { commits: window.draftPerformanceCommits - before, writes };
      });
      assert.deepEqual(
        noOp,
        { commits: 0, writes: 0 },
        "Unchanged remote text preserves consumer state and local storage",
      );
      await page.evaluate(async () => {
        await window.draftPerformanceDb.drafts.incrementalUpsert({
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
          window.draftPerformanceDraft.drafts["chat-0"] === "typed 19" &&
          window.draftPerformanceDraft.conflicts.some(
            (version) => version.text === "Concurrent remote text",
          ),
      );
      const remote = await page.evaluate(
        () =>
          window.draftPerformanceDraft.conflicts.find(
            (version) => version.text === "Concurrent remote text",
          )?.text ?? "",
      );
      await page.evaluate((text) => {
        window.draftPerformanceDraft.setDrafts((current) => ({
          ...current,
          "chat-0": text,
        }));
      }, remote);
      await page.waitForFunction(
        () =>
          window.draftPerformanceDraft.drafts["chat-0"] ===
          "Concurrent remote text",
      );
      await page.waitForFunction(
        () =>
          window.draftPerformanceDraft.drafts["chat-0"] ===
            "Concurrent remote text" &&
          !window.draftPerformanceDraft.conflicts.some(
            (v) => v.text === "typed 19",
          ),
      );
      assert.equal(
        await page.evaluate(
          () => window.draftPerformanceDraft.drafts["chat-499"],
        ),
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
      await installDraftSyncRoutes(other, workspaceId);

      await other.goto(`${fixtureOrigin(server)}/check`);
      await other.evaluate(async () =>
        (
          await import("./draft-render-performance.fixture.tsx")
        ).mountOtherDraftFixture(),
      );
      await other.waitForFunction(
        () =>
          window.draftPerformanceDraft?.drafts["chat-0"] ===
          "Concurrent remote text",
      );
      await other.evaluate(() =>
        window.draftPerformanceDraft.setDrafts((old) => ({
          ...old,
          "chat-1": "Exact second tab edit",
        })),
      );
      await page.waitForFunction(
        () =>
          window.draftPerformanceDraft.drafts["chat-1"] ===
          "Exact second tab edit",
      );
      assert.equal(
        await page.evaluate(
          () => window.draftPerformanceDraft.drafts["chat-0"],
        ),
        "Concurrent remote text",
        "A second tab edit does not replace another chat",
      );
      assert.equal(
        await other.evaluate(() =>
          window.draftPerformanceDraft.conflicts.some(
            (version) => version.text === "typed 19",
          ),
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

async function installDraftSyncRoutes(page: Page, workspaceId: string) {
  await page.route("**/api/sync/identity", (r) =>
    r.fulfill({
      json: { workspaceId } satisfies Pick<
        components["schemas"]["SyncIdentityResponse"],
        "workspaceId"
      >,
      headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
    }),
  );
  await page.route("**/api/sync/drafts", (r) => r.fulfill({ json: [] }));
  await page.route("**/api/sync/pull?*", (r) =>
    r.fulfill({
      json: {
        workspaceId,
        documents: [],
        checkpoint: { seq: 0 },
      } satisfies Pick<
        components["schemas"]["SyncPullResponse"],
        "workspaceId" | "documents" | "checkpoint"
      >,
    }),
  );
  await page.route("**/api/sync/stream*", (r) =>
    r.fulfill({
      contentType: "text/event-stream",
      body: apiSchemaHandshakeSse(),
    }),
  );
}
