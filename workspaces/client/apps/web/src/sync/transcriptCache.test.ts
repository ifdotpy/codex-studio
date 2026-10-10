import { afterEach, beforeEach, expect, it, vi } from "vitest";

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  vi.stubGlobal("document", { hidden: true });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("applies every delta immediately and publishes the latest transcript once", async () => {
  const cache = await import("./transcriptCache");
  cache.cacheTranscriptValue(
    "workspace",
    "a",
    { items: [{ id: "a", text: "initial" }] },
    1,
  );
  const accept = vi.fn();
  const stop = cache.subscribeTranscript("workspace", "a", accept);
  expect(accept).toHaveBeenCalledOnce();
  cache.patchTranscriptValue(
    "workspace",
    "a",
    {},
    2,
    [{ id: "a", text: "second" }],
    [],
  );
  cache.patchTranscriptValue(
    "workspace",
    "a",
    {},
    3,
    [{ id: "b", text: "third", toolStatus: "running" }],
    ["a"],
  );
  cache.patchTranscriptValue(
    "workspace",
    "a",
    { completed: true },
    4,
    [{ id: "b", text: "finished", toolStatus: "completed" }],
    [],
  );
  expect(cache.peekTranscript("workspace", "a")).toEqual({
    seq: 4,
    payload: {
      completed: true,
      items: [{ id: "b", text: "finished", toolStatus: "completed" }],
    },
  });
  expect(accept).toHaveBeenCalledOnce();
  vi.advanceTimersByTime(50);
  expect(accept).toHaveBeenCalledTimes(2);
  expect(accept.mock.calls[1][0].seq).toBe(4);
  expect(accept.mock.calls[1][0].payload.items[0].text).toBe("finished");
  stop();
});

it("keeps workspace notifications separate and cancels notifications for a closed chat", async () => {
  const cache = await import("./transcriptCache");
  const old = vi.fn();
  const current = vi.fn();
  const stopOld = cache.subscribeTranscript("old", "same", old);
  const stopCurrent = cache.subscribeTranscript("current", "same", current);
  cache.cacheTranscriptValue("old", "same", { items: [] }, 100);
  cache.cacheTranscriptValue("current", "same", { items: [{ id: "new" }] }, 1);
  stopOld();
  vi.advanceTimersByTime(50);
  expect(old).not.toHaveBeenCalled();
  expect(current).toHaveBeenCalledOnce();
  expect(current.mock.calls[0][0].payload.items).toEqual([{ id: "new" }]);
  stopCurrent();
});

it("does not publish a cached transcript after it is cleared", async () => {
  const cache = await import("./transcriptCache");
  const accept = vi.fn();
  const stop = cache.subscribeTranscript("workspace", "a", accept);
  cache.cacheTranscriptValue("workspace", "a", { items: [] }, 1);
  cache.clearTranscript("workspace", "a");
  vi.advanceTimersByTime(50);
  expect(accept).not.toHaveBeenCalled();
  stop();
});
