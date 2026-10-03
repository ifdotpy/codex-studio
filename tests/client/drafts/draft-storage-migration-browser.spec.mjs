// Real browser storage verifies interrupted legacy migration and reload recovery.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

import { test, expect, browserExecutablePath } from "../playwright.mjs";

test(
  "draft-storage-migration-browser",
  async () => {
    const require = createRequire(
      new URL("../../../web/package.json", import.meta.url),
    );
    const { chromium } = require("playwright");
    const { createServer } = await import(
      new URL(
        "../../../web/node_modules/vite/dist/node/index.js",
        import.meta.url,
      )
    );
    const server = await createServer({
      configFile: false,
      root: fileURLToPath(new URL("../../../web", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
    });
    await server.listen();
    const browser = await chromium.launch({
      executablePath: browserExecutablePath,
      headless: true,
    });
    try {
      const context = await browser.newContext();
      const page = await context.newPage();
      const errors = [];
      page.on("pageerror", (error) => errors.push(error.message));
      const workspaceId = "c".repeat(32);
      const legacy = {
        lead: "Legacy lead",
        second: "Legacy second",
        blank: "",
      };
      await page.route("**/check", (route) =>
        route.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Draft migration</title>",
        }),
      );
      await page.route("**/api/sync/identity", (route) =>
        route.fulfill({ json: { workspaceId } }),
      );
      await page.route("**/api/sync/drafts", (route) =>
        route.fulfill({ json: [] }),
      );
      await page.route("**/api/sync/pull?*", (route) =>
        route.fulfill({
          json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
        }),
      );
      const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
      await page.goto(origin + "/check");
      await page.evaluate(
        ({ workspaceId, legacy }) => {
          localStorage.setItem(
            "codex-sync-workspace",
            JSON.stringify(workspaceId),
          );
          localStorage.setItem(
            `codex-drafts:${workspaceId}`,
            JSON.stringify(legacy),
          );
          const original = Storage.prototype.setItem;
          let successfulWrites = 1;
          Storage.prototype.setItem = function (key, value) {
            if (key.startsWith("codex-chat-draft:") && successfulWrites-- === 0)
              throw new Error("Injected migration crash");
            return original.call(this, key, value);
          };
        },
        { workspaceId, legacy },
      );
      const mount = async (target = page) =>
        target.evaluate(async () => {
          const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
          const r = await import("/node_modules/.vite/deps/react.js");
          const d =
            await import("/node_modules/.vite/deps/react-dom_client.js");
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
      await mount();
      await page.waitForFunction(
        () =>
          window.draft?.drafts.lead === "Legacy lead" && !!window.draft.error,
      );
      assert.deepEqual(
        await page.evaluate(() => window.draft.drafts),
        legacy,
        "The unmodified legacy map recovers every entry after a partial copy",
      );
      const existingDevice = await page.evaluate(() =>
        JSON.parse(localStorage.getItem("codex-draft-device")),
      );
      await page.reload();
      await mount();
      try {
        await page.waitForFunction(
          () =>
            window.draft?.drafts.second === "Legacy second" &&
            !window.draft.error,
          null,
        );
      } catch (error) {
        process.stderr.write(
          `reload recovery state: ${JSON.stringify({
            browser: await page.evaluate(() => ({
              draft: window.draft?.drafts,
              error: window.draft?.error,
              legacy: localStorage.getItem(`codex-drafts:${"c".repeat(32)}`),
              keys: Object.keys(localStorage).filter((key) =>
                key.startsWith("codex-chat-draft:"),
              ),
            })),
            pageErrors: errors,
          })}\n`,
        );
        throw error;
      }
      const result = await page.evaluate((workspaceId) => {
        const keys = Object.keys(localStorage).filter((key) =>
          key.startsWith(`codex-chat-draft:${workspaceId}:`),
        );
        return {
          keys,
          legacy: localStorage.getItem(`codex-drafts:${workspaceId}`),
          drafts: window.draft.drafts,
          device: JSON.parse(localStorage.getItem("codex-draft-device")),
        };
      }, workspaceId);
      assert.equal(
        result.keys.length,
        3,
        "Retry completes one durable record per chat",
      );
      assert.equal(
        result.legacy,
        JSON.stringify(legacy),
        "Legacy source remains intact",
      );
      assert.deepEqual(result.drafts, legacy);
      assert.equal(
        result.device,
        existingDevice,
        "An existing device retains its legacy device identity across reload",
      );
      const legacyTab = await context.newPage();
      await legacyTab.route("**/check", (route) =>
        route.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Legacy tab</title>",
        }),
      );
      await legacyTab.goto(origin + "/check");
      const editedByOldTab = { ...legacy, second: "Changed in older tab" };
      await legacyTab.evaluate(
        ({ workspaceId, value }) =>
          localStorage.setItem(
            `codex-drafts:${workspaceId}`,
            JSON.stringify(value),
          ),
        { workspaceId, value: editedByOldTab },
      );
      await page.waitForFunction(
        () => window.draft?.drafts.second === "Changed in older tab",
      );
      const pendingLegacyUpdate = await page.evaluate(async () => {
        const pending = Object.keys(localStorage)
          .filter((key) => key.includes(":pending:"))
          .map((key) => JSON.parse(localStorage.getItem(key)));
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        const stored = (await db.drafts.find().exec()).map((doc) =>
          JSON.parse(doc.payload),
        );
        return [...pending, ...stored];
      });
      assert.ok(
        pendingLegacyUpdate.some(
          (entry) => entry.text === "Changed in older tab",
        ),
        "An old-tab update enters the new version journal",
      );
      const lateSession = "old-tab-new-chat";
      const lateText = "Chat first created in an already-open old tab";
      const lateJournalKey = `codex-drafts:${workspaceId}:pending:legacy:${encodeURIComponent(lateSession)}`;
      const adoptionMarker = await page.evaluate((journalKey) => {
        const marker = crypto.randomUUID();
        window.lateSessionAdoptionMarker = marker;
        window.lateSessionJournalWrites = [];
        const original = Storage.prototype.setItem;
        Storage.prototype.setItem = function (key, value) {
          if (key === journalKey)
            window.lateSessionJournalWrites.push(JSON.parse(value));
          return original.call(this, key, value);
        };
        return marker;
      }, lateJournalKey);
      await page.evaluate((scope) => {
        window.legacyStorageEvents = 0;
        window.addEventListener("storage", (event) => {
          if (event.key === scope) window.legacyStorageEvents++;
        });
      }, `codex-drafts:${workspaceId}`);
      await legacyTab.evaluate(
        ({ workspaceId, session, text }) => {
          const key = `codex-drafts:${workspaceId}`;
          const map = JSON.parse(localStorage.getItem(key));
          map[session] = text;
          localStorage.setItem(key, JSON.stringify(map));
        },
        { workspaceId, session: lateSession, text: lateText },
      );
      await page.waitForFunction(
        async ({ session, text, device }) => {
          if (window.draft?.drafts[session] !== text) return false;
          const { db } = await (
            await import("/src/sync/client.ts")
          ).syncDatabase();
          const document = await db.drafts
            .findOne(`${device}:${session}`)
            .exec();
          if (!document || document.id !== `${device}:${session}`) return false;
          const payload = JSON.parse(document.payload);
          return (
            payload.id === document.id &&
            payload.session === session &&
            payload.text === text
          );
        },
        { session: lateSession, text: lateText, device: existingDevice },
      );
      const lateRecord = await page.evaluate(
        ({ workspaceId, session }) => {
          const record = JSON.parse(
            localStorage.getItem(`codex-chat-draft:${workspaceId}:${session}`),
          );
          return {
            record,
            storageEvents: window.legacyStorageEvents,
            marker: window.lateSessionAdoptionMarker,
            journalWrites: window.lateSessionJournalWrites,
          };
        },
        { workspaceId, session: lateSession },
      );
      assert.equal(lateRecord.storageEvents, 1);
      assert.equal(
        lateRecord.marker,
        adoptionMarker,
        "The old-tab addition is adopted without reloading the open new client",
      );
      assert.equal(lateRecord.record.source, "local");
      assert.equal(lateRecord.record.legacyPending, false);
      assert.equal(lateRecord.record.legacyBaseline, undefined);
      assert.equal(typeof lateRecord.record.legacyBaselineHash, "string");
      assert.ok(
        lateRecord.journalWrites.some(
          (entry) =>
            entry.session === lateSession &&
            entry.id === `${existingDevice}:${lateSession}` &&
            entry.text === lateText,
        ),
        "The exact old-tab draft was written to the durable journal",
      );
      const lateDocumentCount = await page.evaluate(async (session) => {
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        return (await db.drafts.find().exec()).filter((document) =>
          document.id.endsWith(`:${session}`),
        ).length;
      }, lateSession);
      assert.equal(
        lateDocumentCount,
        1,
        "Adoption creates one logical RxDB row",
      );

      await page.reload();
      await mount();
      await page.waitForFunction(
        ({ session, text }) => window.draft?.drafts[session] === text,
        { session: lateSession, text: lateText },
      );
      await page.evaluate((scope) => {
        window.legacyStorageEvents = 0;
        window.addEventListener("storage", (event) => {
          if (event.key === scope) window.legacyStorageEvents++;
        });
      }, `codex-drafts:${workspaceId}`);

      const retryWithoutEvent =
        "Old-tab edit retried after journal quota failure";
      const checkpointBeforeFailure = await page.evaluate(
        ({ workspaceId, session }) =>
          JSON.parse(
            localStorage.getItem(
              `codex-chat-draft:${workspaceId}:${encodeURIComponent(session)}`,
            ),
          ).legacyBaselineHash,
        { workspaceId, session: lateSession },
      );
      const failedLegacyJournalKey = lateJournalKey;
      await page.evaluate((key) => {
        const original = Storage.prototype.setItem;
        window.legacyJournalWriteFailed = false;
        window.legacyJournalWriteAttempts = 0;
        window.allowLegacyJournalRetry = false;
        window.checkpointAtInjectedJournalFailure = null;
        Storage.prototype.setItem = function (candidate, value) {
          if (candidate === key) {
            window.legacyJournalWriteAttempts++;
            if (!window.allowLegacyJournalRetry) {
              if (!window.legacyJournalWriteFailed) {
                window.legacyJournalWriteFailed = true;
                const [, workspaceId] = candidate.split(":");
                const session = decodeURIComponent(
                  candidate.split(":pending:legacy:")[1],
                );
                window.checkpointAtInjectedJournalFailure = JSON.parse(
                  localStorage.getItem(
                    `codex-chat-draft:${workspaceId}:${encodeURIComponent(session)}`,
                  ),
                ).legacyBaselineHash;
              }
              throw new DOMException(
                "Injected quota failure held until visible error observation",
                "QuotaExceededError",
              );
            }
          }
          return original.call(this, candidate, value);
        };
      }, failedLegacyJournalKey);
      // Keep React's error observation on the active headless page while the old
      // tab generates the storage event; Chromium throttles RAF in background tabs.
      await page.bringToFront();
      await legacyTab.evaluate(
        ({ workspaceId, session, text }) => {
          const key = `codex-drafts:${workspaceId}`;
          const map = JSON.parse(localStorage.getItem(key));
          map[session] = text;
          localStorage.setItem(key, JSON.stringify(map));
        },
        { workspaceId, session: lateSession, text: retryWithoutEvent },
      );
      await page.waitForFunction(() => window.legacyStorageEvents === 1);
      await page.waitForFunction(
        ({ session, text }) =>
          window.legacyJournalWriteFailed &&
          window.draft?.drafts[session] === text &&
          !!window.draft.error,
        { session: lateSession, text: retryWithoutEvent },
        // The original page is backgrounded by the old-tab fixture; RAF polling
        // can otherwise miss the visible error until after the 3s retry fires.
        { polling: 25, timeout: 10_000 },
      );
      await page.waitForFunction(
        () => window.legacyJournalWriteAttempts >= 2,
        null,
        { polling: 50, timeout: 10_000 },
      );
      const failedJournalAttempts = await page.evaluate(
        () => window.legacyJournalWriteAttempts,
      );
      assert.ok(
        failedJournalAttempts >= 2,
        "The injected storage failure remains held through multiple retries",
      );
      assert.equal(
        await page.evaluate(() => window.allowLegacyJournalRetry),
        false,
        "Retries remain blocked until the test releases storage",
      );
      assert.equal(
        await page.evaluate(
          ({ workspaceId, session }) =>
            JSON.parse(
              localStorage.getItem(
                `codex-chat-draft:${workspaceId}:${encodeURIComponent(session)}`,
              ),
            ).legacyBaselineHash,
          { workspaceId, session: lateSession },
        ),
        checkpointBeforeFailure,
        "Repeated journal failures leave the checkpoint unchanged",
      );
      await page.evaluate(() => {
        if (!window.draft?.error)
          throw new Error("The injected journal failure was not visible yet");
        window.legacyJournalErrorObserved = true;
        window.allowLegacyJournalRetry = true;
      });
      assert.equal(
        await page.evaluate(() => window.checkpointAtInjectedJournalFailure),
        checkpointBeforeFailure,
        "Failed journal persistence does not advance the legacy checkpoint",
      );
      const retryCheckpoint = await page.evaluate(
        async ({ session, text }) =>
          (await import("/src/sync/draftStorage.ts")).legacyBaselineHash(
            session,
            text,
          ),
        { session: lateSession, text: retryWithoutEvent },
      );
      const retryDeadline = Date.now() + 15_000;
      let retryState;
      let retryConverged = false;
      while (Date.now() < retryDeadline) {
        retryState = await page.evaluate(
          async ({ workspaceId, session }) => {
            const { db } = await (
              await import("/src/sync/client.ts")
            ).syncDatabase();
            const local = JSON.parse(
              localStorage.getItem(
                `codex-chat-draft:${workspaceId}:${encodeURIComponent(session)}`,
              ),
            );
            const rows = (await db.drafts.find().exec())
              .filter((document) => document.id.endsWith(`:${session}`))
              .map((document) => ({
                id: document.id,
                payload: JSON.parse(document.payload),
              }));
            return {
              checkpoint: local?.legacyBaselineHash,
              events: window.legacyStorageEvents,
              attempts: window.legacyJournalWriteAttempts,
              rows,
            };
          },
          { workspaceId, session: lateSession },
        );
        const [row] = retryState.rows;
        if (
          retryState.events === 1 &&
          retryState.attempts > failedJournalAttempts &&
          retryState.checkpoint === retryCheckpoint &&
          retryState.rows.length === 1 &&
          row.id === `${existingDevice}:${lateSession}` &&
          row.payload.id === row.id &&
          row.payload.session === lateSession &&
          row.payload.text === retryWithoutEvent
        ) {
          retryConverged = true;
          break;
        }
        await page.waitForTimeout(50);
      }
      assert.ok(
        retryConverged,
        `The released journal retry converges without another event or reload: ${JSON.stringify(retryState)}`,
      );
      assert.equal(
        await page.evaluate(() => window.legacyStorageEvents),
        1,
        "The journal retry succeeds without another old-tab event or reload",
      );
      assert.ok(
        (await page.evaluate(() => window.legacyJournalWriteAttempts)) >
          failedJournalAttempts,
        "A post-release retry writes the failed legacy journal again",
      );
      assert.equal(
        await page.evaluate(
          ({ workspaceId, session }) =>
            JSON.parse(
              localStorage.getItem(
                `codex-chat-draft:${workspaceId}:${encodeURIComponent(session)}`,
              ),
            ).legacyBaselineHash,
          { workspaceId, session: lateSession },
        ),
        retryCheckpoint,
        "The checkpoint advances after the retry persists the journal",
      );
      const emptySession = "old-tab-empty-chat";
      const emptyJournalKey = `codex-drafts:${workspaceId}:pending:legacy:${encodeURIComponent(emptySession)}`;
      await page.evaluate((key) => {
        window.emptyChatJournalWrites = [];
        const original = Storage.prototype.setItem;
        Storage.prototype.setItem = function (candidate, value) {
          if (candidate === key)
            window.emptyChatJournalWrites.push(JSON.parse(value));
          return original.call(this, candidate, value);
        };
      }, emptyJournalKey);
      await legacyTab.evaluate(
        ({ workspaceId, session }) => {
          const key = `codex-drafts:${workspaceId}`;
          const map = JSON.parse(localStorage.getItem(key));
          map[session] = "";
          localStorage.setItem(key, JSON.stringify(map));
        },
        { workspaceId, session: emptySession },
      );
      await page.waitForFunction(() => window.legacyStorageEvents === 2);
      const emptyRemovalSnapshot = await page.evaluate(
        ({ workspaceId, session }) => ({
          draft: window.draft?.drafts[session],
          record: localStorage.getItem(
            `codex-chat-draft:${workspaceId}:${encodeURIComponent(session)}`,
          ),
          error: window.draft?.error,
        }),
        { workspaceId, session: emptySession },
      );
      assert.ok(
        emptyRemovalSnapshot.record,
        "The adopted empty draft remains durably represented",
      );
      assert.equal(
        emptyRemovalSnapshot.draft,
        "",
        "An empty old-tab draft is not confused with a missing hook value",
      );
      assert.ok(
        emptyRemovalSnapshot.error === "",
        "Empty and absent checkpoint values reconcile without a local error",
      );
      assert.ok(
        await page.evaluate(
          ({ device, session }) =>
            window.emptyChatJournalWrites.some(
              (entry) =>
                entry.session === session &&
                entry.id === `${device}:${session}` &&
                entry.text === "",
            ),
          { device: existingDevice, session: emptySession },
        ),
        "Adding and removing an empty old-tab entry journals its exact chat version",
      );
      await page.waitForFunction(
        async ({ device, session }) => {
          if (window.draft?.drafts[session] !== "") return false;
          const { db } = await (
            await import("/src/sync/client.ts")
          ).syncDatabase();
          const row = await db.drafts.findOne(`${device}:${session}`).exec();
          return row && JSON.parse(row.payload).text === "";
        },
        { device: existingDevice, session: emptySession },
      );
      await legacyTab.evaluate(
        ({ workspaceId, session }) => {
          const key = `codex-drafts:${workspaceId}`;
          const map = JSON.parse(localStorage.getItem(key));
          delete map[session];
          localStorage.setItem(key, JSON.stringify(map));
        },
        { workspaceId, session: emptySession },
      );
      await page.waitForFunction(() => window.legacyStorageEvents === 3);
      await page.waitForFunction(
        () => window.emptyChatJournalWrites.length >= 2,
      );
      const removedEmptyState = await page.evaluate(
        ({ workspaceId, session }) => ({
          draft: window.draft?.drafts[session],
          record: JSON.parse(
            localStorage.getItem(
              `codex-chat-draft:${workspaceId}:${encodeURIComponent(session)}`,
            ),
          ),
          legacyHasEntry: Object.hasOwn(
            JSON.parse(localStorage.getItem(`codex-drafts:${workspaceId}`)),
            session,
          ),
          journalWrites: window.emptyChatJournalWrites,
        }),
        { workspaceId, session: emptySession },
      );
      assert.equal(removedEmptyState.legacyHasEntry, false);
      assert.ok(
        removedEmptyState.draft === undefined || removedEmptyState.draft === "",
      );
      assert.equal(removedEmptyState.record.deleted, true);
      assert.equal(removedEmptyState.record.text, "");
      assert.ok(
        removedEmptyState.journalWrites.some(
          (entry) =>
            entry.session === emptySession &&
            entry.id === `${existingDevice}:${emptySession}` &&
            entry.text === "",
        ),
        "Old-tab removal reconciles as the legacy writer's empty draft branch",
      );
      await legacyTab.evaluate((workspaceId) => {
        localStorage.setItem(
          `codex-drafts:${workspaceId}`,
          JSON.stringify({ lead: "Legacy lead", blank: "" }),
        );
      }, workspaceId);
      await page.waitForFunction(
        () => window.draft?.drafts.second === undefined,
      );
      const removed = await page.evaluate(
        (workspaceId) =>
          JSON.parse(
            localStorage.getItem(`codex-chat-draft:${workspaceId}:second`),
          ),
        workspaceId,
      );
      assert.equal(
        removed.deleted,
        true,
        "Old-tab removal persists a clear tombstone",
      );
      await page.waitForFunction(async () => {
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        return (await db.drafts.find().exec()).some((doc) => {
          const version = JSON.parse(doc.payload);
          return version.session === "second" && version.text === "";
        });
      });
      const locallyEdited = "New tab edit survives an offline old-tab update";
      await page.evaluate((text) => {
        window.draft.setDrafts((old) => ({ ...old, second: text }));
      }, locallyEdited);
      await page.waitForFunction(
        (text) => window.draft?.drafts.second === text,
        locallyEdited,
      );
      const freshOldTabText = "Fresh old-tab edit after the local edit";
      await page.evaluate(async (freshOldTabText) => {
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        const original = db.drafts.incrementalUpsert.bind(db.drafts);
        db.drafts.incrementalUpsert = async (doc) => {
          if (JSON.parse(doc.payload).text === freshOldTabText)
            throw new Error("Injected local sync failure");
          return original(doc);
        };
      }, freshOldTabText);
      await legacyTab.evaluate(
        ({ workspaceId, value }) =>
          localStorage.setItem(
            `codex-drafts:${workspaceId}`,
            JSON.stringify(value),
          ),
        {
          workspaceId,
          value: {
            lead: "Legacy lead",
            second: freshOldTabText,
            blank: "",
          },
        },
      );
      await page.waitForFunction(
        (args) =>
          window.draft?.drafts.second === args.local &&
          window.draft.error.includes("synchronization"),
        { local: locallyEdited, fresh: freshOldTabText },
      );
      const pendingOfflineLegacy = await page.evaluate((freshOldTabText) => {
        const version = Object.keys(localStorage)
          .filter((key) => key.includes(":pending:legacy:"))
          .map((key) => JSON.parse(localStorage.getItem(key)))
          .find((entry) => entry.text === freshOldTabText);
        const record = JSON.parse(
          localStorage.getItem(`codex-chat-draft:${"c".repeat(32)}:second`),
        );
        return { version, record };
      }, freshOldTabText);
      assert.ok(
        pendingOfflineLegacy.version,
        "Fresh old-tab edit is journaled offline",
      );
      assert.equal(pendingOfflineLegacy.version.id, `${existingDevice}:second`);
      assert.equal(pendingOfflineLegacy.record.text, locallyEdited);
      assert.equal(pendingOfflineLegacy.record.legacyBaseline, undefined);
      assert.equal(
        typeof pendingOfflineLegacy.record.legacyBaselineHash,
        "string",
      );
      await page.reload();
      await mount();
      await page.waitForFunction(
        (text) => window.draft?.drafts.second === text,
        locallyEdited,
      );
      await page.waitForFunction(
        async (expected) => {
          const { db } = await (
            await import("/src/sync/client.ts")
          ).syncDatabase();
          return (await db.drafts.find().exec()).some((doc) => {
            const version = JSON.parse(doc.payload);
            return version.id === expected.id && version.text === expected.text;
          });
        },
        { id: `${existingDevice}:second`, text: freshOldTabText },
      );
      await page.waitForFunction(
        (text) => window.draft?.conflicts.some((entry) => entry.text === text),
        freshOldTabText,
      );
      const localClear = await page.evaluate((workspaceId) => {
        const before = window.draft.drafts.second;
        window.draft.setDrafts((old) => {
          const next = { ...old };
          delete next.second;
          return next;
        });
        return {
          before,
          record: JSON.parse(
            localStorage.getItem(`codex-chat-draft:${workspaceId}:second`),
          ),
        };
      }, workspaceId);
      assert.equal(
        localClear.before,
        locallyEdited,
        "The local edit remains the selected value before clearing",
      );
      assert.equal(
        localClear.record.deleted,
        true,
        "Local clear writes a tombstone before the legacy edit",
      );
      await page.waitForFunction(
        () => window.draft?.drafts.second === undefined,
      );
      await legacyTab.evaluate((workspaceId) => {
        localStorage.setItem(
          `codex-drafts:${workspaceId}`,
          JSON.stringify({
            lead: "Legacy lead",
            blank: "",
            second: "Fresh old-tab edit after the local edit",
            unrelated: "Cause another map event",
          }),
        );
      }, workspaceId);
      await page.waitForTimeout(100);
      assert.equal(
        await page.evaluate(() => window.draft.drafts.second || undefined),
        undefined,
        "An unchanged old-map value cannot resurrect a local tombstone",
      );
      const afterTombstoneText = "Another fresh edit from the old tab";
      await legacyTab.evaluate(
        ({ workspaceId, afterTombstoneText }) =>
          localStorage.setItem(
            `codex-drafts:${workspaceId}`,
            JSON.stringify({
              lead: "Legacy lead",
              blank: "",
              second: afterTombstoneText,
              unrelated: "Cause another map event",
            }),
          ),
        { workspaceId, afterTombstoneText },
      );
      await page.waitForFunction(async (text) => {
        if (window.draft?.drafts.second) return false;
        const pending = Object.keys(localStorage)
          .filter((key) => key.includes(":pending:legacy:"))
          .map((key) => JSON.parse(localStorage.getItem(key)));
        if (pending.some((entry) => entry.text === text)) return true;
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        return (await db.drafts.find().exec()).some(
          (doc) => JSON.parse(doc.payload).text === text,
        );
      }, afterTombstoneText);
      await page.waitForTimeout(250);
      const tombstoneBranch = await page.evaluate(
        async (afterTombstoneText) => {
          const record = JSON.parse(
            localStorage.getItem(`codex-chat-draft:${"c".repeat(32)}:second`),
          );
          const pendingBranch = Object.keys(localStorage)
            .filter((key) => key.includes(":pending:legacy:"))
            .map((key) => JSON.parse(localStorage.getItem(key)))
            .find((entry) => entry.text === afterTombstoneText);
          const { db } = await (
            await import("/src/sync/client.ts")
          ).syncDatabase();
          const branch =
            pendingBranch ||
            (await db.drafts.find().exec())
              .map((doc) => JSON.parse(doc.payload))
              .find(
                (entry) =>
                  entry.id.endsWith(":second") &&
                  entry.text === afterTombstoneText,
              );
          return { record, branch };
        },
        afterTombstoneText,
      );
      assert.equal(
        tombstoneBranch.record.deleted,
        true,
        "A fresh old-tab edit cannot replace the tombstone",
      );
      assert.ok(
        tombstoneBranch.branch,
        "Changed old-tab text remains an independent branch after a tombstone",
      );

      const corruptContext = await browser.newContext();
      const corruptPage = await corruptContext.newPage();
      const corruptErrors = [];
      corruptPage.on("pageerror", (error) => corruptErrors.push(error.message));
      await corruptPage.route("**/check", (route) =>
        route.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Corrupt draft key</title>",
        }),
      );
      await corruptPage.route("**/api/sync/identity", (route) =>
        route.fulfill({ json: { workspaceId } }),
      );
      await corruptPage.route("**/api/sync/drafts", (route) =>
        route.fulfill({ json: [] }),
      );
      await corruptPage.route("**/api/sync/pull?*", (route) =>
        route.fulfill({
          json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
        }),
      );
      await corruptPage.goto(origin + "/check");
      await corruptPage.evaluate((workspaceId) => {
        localStorage.setItem(
          "codex-sync-workspace",
          JSON.stringify(workspaceId),
        );
        localStorage.setItem(
          `codex-chat-draft:${workspaceId}:healthy`,
          JSON.stringify({
            version: 1,
            workspace: workspaceId,
            session: "healthy",
            text: "Healthy chat survives malformed key",
            deleted: false,
            source: "local",
          }),
        );
        localStorage.setItem(`codex-chat-draft:${workspaceId}:%E0%A4%A`, "{}");
        localStorage.setItem(
          `codex-drafts:${workspaceId}`,
          JSON.stringify({ legacy: "Legacy fallback remains visible" }),
        );
      }, workspaceId);
      await mount(corruptPage);
      await corruptPage.waitForFunction(
        () =>
          window.draft?.drafts.healthy ===
            "Healthy chat survives malformed key" &&
          window.draft.drafts.legacy === "Legacy fallback remains visible" &&
          !!window.draft.error,
      );
      assert.deepEqual(
        corruptErrors,
        [],
        "One malformed suffix does not crash draft-hook initialization",
      );
      await corruptContext.close();
      expect(errors).toEqual([]);
      console.log(
        "PASS: partial migration/reload recovery; old-tab edits/removal cross the version journal and durable tombstones.",
      );
    } finally {
      await browser.close();
      await server.close();
    }
  },
  { timeout: 120_000 },
);
