import { readFileSync } from "node:fs";
import { transform } from "esbuild";
import { chromium } from "playwright";
import { afterEach, expect, it, onTestFinished, vi } from "vitest";
import {
  createMonitorInputStore,
  MONITOR_INPUT_DATABASE_NAME,
} from "./monitorInputStore";

afterEach(() => vi.unstubAllGlobals());
// Total budget for the browser fixture, including launch and event barriers.
const BROWSER_TEST_TIMEOUT_MS = 10_000;
const barriers = {
  snapshot: "monitor-input:snapshot-read",
  queued: "monitor-input:transaction-queued",
};

it("reports a blocked database and closes a late connection without using it", async () => {
  const request = {} as IDBOpenDBRequest;
  vi.stubGlobal("indexedDB", { open: () => request });
  const pending = createMonitorInputStore("local").transition(
    "monitor",
    () => ({ entry: undefined, result: undefined }),
  );
  const rejected = expect(pending).rejects.toThrow("journal is blocked");
  request.onblocked!(new Event("blocked") as IDBVersionChangeEvent);
  await rejected;
  const db = { close: vi.fn(), transaction: vi.fn() };
  Object.defineProperty(request, "result", { value: db });
  request.onsuccess!(new Event("success"));
  expect(db.close).toHaveBeenCalledOnce();
  expect(db.transaction).not.toHaveBeenCalled();
});

it(
  "serializes committed monitor metadata across two HTTP browser contexts without Web Locks",
  async () => {
    const deadline = performance.now() + BROWSER_TEST_TIMEOUT_MS;
    const remaining = () => Math.max(1, deadline - performance.now());
    const source = readFileSync(
      new URL("./monitorInputStore.ts", import.meta.url),
      "utf8",
    );
    const { code } = await transform(source, {
      loader: "ts",
      format: "iife",
      globalName: "inputStore",
    });
    const browser = await chromium.launch({
      headless: true,
      timeout: remaining(),
      ...(process.env.CHROME_BIN
        ? { executablePath: process.env.CHROME_BIN }
        : {}),
    });
    // Vitest runs this hook even when its outer timeout interrupts the test.
    onTestFinished(() => browser.close());
    let releaseHold: (() => Promise<unknown>) | undefined;
    try {
      const context = await browser.newContext();
      await context.route("http://studio-monitor-fixture.invalid/**", (route) =>
        route.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Monitor transaction fixture</title>",
        }),
      );
      const first = await context.newPage();
      const second = await context.newPage();
      releaseHold = () =>
        first.evaluate(() => {
          const fixture = window as any;
          fixture.hold = false;
          clearTimeout(fixture.holdTimer);
        });
      for (const page of [first, second]) {
        vi.spyOn(page, "waitForFunction").mockImplementation(() => {
          throw new Error(
            "The IndexedDB fixture must use event barriers, not polling.",
          );
        });
      }
      for (const page of [first, second]) {
        await page.goto("http://studio-monitor-fixture.invalid/");
        await page.addScriptTag({ content: code });
        await page.evaluate((barriers) => {
          const fixture = window as any;
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
          const fixture = window as any;
          await fixture.store.transition("monitor", () => ({
            entry: fixture.entryA,
            result: undefined,
          }));
          const put = IDBObjectStore.prototype.put;
          IDBObjectStore.prototype.put = function (value, key) {
            const request = put.call(this, value, key!);
            if (fixture.hold && value.requestId === "A") {
              console.debug(fixture.barriers.snapshot);
              const keepTransactionActive = () => {
                if (fixture.hold)
                  this.get(key!).onsuccess = keepTransactionActive;
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
          fixture.oldRetry = fixture.store.transition(
            "monitor",
            (entry: unknown) => ({ entry, result: "A retry committed" }),
          );
        }, remaining()),
      ]);
      const queued = second.waitForEvent("console", {
        predicate: (message) => message.text() === barriers.queued,
        timeout: remaining(),
      });
      const newInput = second.evaluate(async () => {
        const fixture = window as any;
        const transaction = IDBDatabase.prototype.transaction;
        IDBDatabase.prototype.transaction = function (...args) {
          const result = transaction.apply(this, args as any);
          if (!fixture.queued) {
            fixture.queued = true;
            console.debug(fixture.barriers.queued);
          }
          return result;
        };
        fixture.settled = false;
        await fixture.store.transition("monitor", (entry: any) => ({
          entry: entry?.requestId === "A" ? undefined : entry,
          result: undefined,
        }));
        fixture.settled = true;
        await fixture.store.transition("monitor", () => ({
          entry: fixture.entryB,
          result: undefined,
        }));
      });
      // A failed assertion still closes the browser and observes this promise.
      void newInput.catch(() => {});
      await queued;
      expect(await second.evaluate(() => (window as any).settled)).toBe(false);
      await first.evaluate(async () => {
        const fixture = window as any;
        fixture.hold = false;
        clearTimeout(fixture.holdTimer);
        await fixture.oldRetry;
        if (fixture.holdExpired)
          throw new Error("The IndexedDB transaction hold expired.");
      });
      await newInput;
      const result = await first.evaluate(async () => {
        const fixture = window as any;
        // A late A completion must not remove the newer unresolved B.
        await fixture.store.transition("monitor", (entry: any) => ({
          entry: entry?.requestId === "A" ? undefined : entry,
          result: undefined,
        }));
        const reloaded = fixture.inputStore.createMonitorInputStore("local");
        const current = await reloaded.transition(
          "monitor",
          (entry: unknown) => ({ entry, result: entry }),
        );
        const remote = fixture.inputStore.createMonitorInputStore("remote");
        const separate = await remote.transition(
          "monitor",
          (entry: unknown) => ({
            entry,
            result: entry ?? null,
          }),
        );
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
        const fixture = window as any;
        const put = IDBObjectStore.prototype.put;
        IDBObjectStore.prototype.put = function (value, key) {
          const request = put.call(this, value, key!);
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
          (entry: unknown) => ({ entry, result: entry ?? null }),
        );
        return { refused, after };
      });
      expect(aborted).toEqual({ refused: true, after: null });
      expect(MONITOR_INPUT_DATABASE_NAME).toBe(
        "studio-monitor-input-journal-v1",
      );
      for (const page of [first, second])
        expect(page.waitForFunction).not.toHaveBeenCalled();
    } finally {
      await releaseHold?.().catch(() => {});
      await browser.close();
    }
  },
  BROWSER_TEST_TIMEOUT_MS,
);
