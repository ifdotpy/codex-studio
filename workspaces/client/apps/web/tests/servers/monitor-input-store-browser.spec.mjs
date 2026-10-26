import { readFileSync } from "node:fs";
import { transform } from "esbuild";
import { test, expect } from "../playwright.mjs";
import { MONITOR_INPUT_DATABASE_NAME } from "../../src/servers/monitorInputStore.ts";

// Manual browser integration. The short tier exercises IDB event boundaries
// without a browser. This scenario uses real browser transaction serialization.
// Playwright owns the context and browser lifecycle.
const BROWSER_TEST_TIMEOUT_MS = 10_000;
const barriers = {
  snapshot: "monitor-input:snapshot-read",
  queued: "monitor-input:transaction-queued",
};

test("serializes committed monitor metadata across two HTTP browser pages without Web Locks", async ({
  context,
}) => {
  test.setTimeout(BROWSER_TEST_TIMEOUT_MS);
  const deadline = performance.now() + BROWSER_TEST_TIMEOUT_MS;
  const remaining = () => Math.max(1, deadline - performance.now());
  const source = readFileSync(
    new URL("../../src/servers/monitorInputStore.ts", import.meta.url),
    "utf8",
  );
  const { code } = await transform(source, {
    loader: "ts",
    format: "iife",
    globalName: "inputStore",
  });

  await context.route("http://studio-monitor-fixture.invalid/**", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Monitor transaction fixture</title>",
    }),
  );
  const first = await context.newPage();
  const second = await context.newPage();
  let pollingCalls = 0;
  for (const page of [first, second]) {
    page.waitForFunction = () => {
      pollingCalls += 1;
      throw new Error(
        "The IndexedDB fixture must use event barriers, not polling.",
      );
    };
  }
  for (const page of [first, second]) {
    await page.goto("http://studio-monitor-fixture.invalid/");
    await page.addScriptTag({ content: code });
    await page.evaluate((barriers) => {
      const fixture = window;
      fixture.barriers = barriers;
      Object.defineProperty(navigator, "locks", { value: undefined });
      fixture.store = fixture.inputStore.createMonitorInputStore("local");
      fixture.entryA = {
        requestId: "A",
        payloadDigest: "a".repeat(64),
        uncertain: true,
      };
      fixture.entryB = {
        requestId: "B",
        payloadDigest: "b".repeat(64),
        uncertain: true,
      };
    }, barriers);
  }
  expect(
    await first.evaluate(() => ({
      secure: isSecureContext,
      locks: !!navigator.locks,
    })),
  ).toEqual({ secure: false, locks: false });
  const snapshotRead = first.waitForEvent("console", {
    predicate: (message) => message.text() === barriers.snapshot,
    timeout: remaining(),
  });
  await Promise.all([
    snapshotRead,
    first.evaluate(async (holdBudgetMs) => {
      const fixture = window;
      await fixture.store.transition("monitor", () => ({
        entry: fixture.entryA,
        result: undefined,
      }));
      const put = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function (value, key) {
        const request = put.call(this, value, key);
        if (fixture.hold && value.requestId === "A") {
          console.debug(fixture.barriers.snapshot);
          const keepTransactionActive = () => {
            if (fixture.hold) this.get(key).onsuccess = keepTransactionActive;
          };
          keepTransactionActive();
        }
        return request;
      };
      fixture.hold = true;
      fixture.holdTimer = setTimeout(() => {
        fixture.holdExpired = true;
        fixture.hold = false;
      }, holdBudgetMs);
      fixture.oldRetry = fixture.store.transition("monitor", (entry) => ({
        entry,
        result: "A retry committed",
      }));
    }, remaining()),
  ]);
  const queued = second.waitForEvent("console", {
    predicate: (message) => message.text() === barriers.queued,
    timeout: remaining(),
  });
  const newInput = second.evaluate(async () => {
    const fixture = window;
    const transaction = IDBDatabase.prototype.transaction;
    IDBDatabase.prototype.transaction = function (...args) {
      const result = transaction.apply(this, args);
      if (!fixture.queued) {
        fixture.queued = true;
        console.debug(fixture.barriers.queued);
      }
      return result;
    };
    fixture.settled = false;
    await fixture.store.transition("monitor", (entry) => ({
      entry: entry?.requestId === "A" ? undefined : entry,
      result: undefined,
    }));
    fixture.settled = true;
    await fixture.store.transition("monitor", () => ({
      entry: fixture.entryB,
      result: undefined,
    }));
  });
  // Playwright closes the context after a failed assertion. Observe this promise.
  void newInput.catch(() => {});
  await queued;
  expect(await second.evaluate(() => window.settled)).toBe(false);
  await first.evaluate(async () => {
    const fixture = window;
    fixture.hold = false;
    clearTimeout(fixture.holdTimer);
    await fixture.oldRetry;
    if (fixture.holdExpired)
      throw new Error("The IndexedDB transaction hold expired.");
  });
  await newInput;
  const result = await first.evaluate(async () => {
    const fixture = window;
    // A late A completion must not remove the newer unresolved B.
    await fixture.store.transition("monitor", (entry) => ({
      entry: entry?.requestId === "A" ? undefined : entry,
      result: undefined,
    }));
    const reloaded = fixture.inputStore.createMonitorInputStore("local");
    const current = await reloaded.transition("monitor", (entry) => ({
      entry,
      result: entry,
    }));
    const remote = fixture.inputStore.createMonitorInputStore("remote");
    const separate = await remote.transition("monitor", (entry) => ({
      entry,
      result: entry ?? null,
    }));
    return { current, separate };
  });
  expect(result).toEqual({
    current: {
      requestId: "B",
      payloadDigest: "b".repeat(64),
      uncertain: true,
    },
    separate: null,
  });
  const aborted = await second.evaluate(async () => {
    const fixture = window;
    const put = IDBObjectStore.prototype.put;
    IDBObjectStore.prototype.put = function (value, key) {
      const request = put.call(this, value, key);
      this.transaction.abort();
      return request;
    };
    let refused = false;
    try {
      await fixture.store.transition("aborted-monitor", () => ({
        entry: fixture.entryA,
        result: undefined,
      }));
    } catch {
      refused = true;
    } finally {
      IDBObjectStore.prototype.put = put;
    }
    const after = await fixture.store.transition(
      "aborted-monitor",
      (entry) => ({ entry, result: entry ?? null }),
    );
    return { refused, after };
  });
  expect(aborted).toEqual({ refused: true, after: null });
  expect(MONITOR_INPUT_DATABASE_NAME).toBe("studio-monitor-input-journal-v1");
  expect(pollingCalls).toBe(0);
});
