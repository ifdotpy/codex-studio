import { afterEach, beforeEach, expect, it, vi } from "vitest";

vi.mock("./client", () => ({}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: vi.fn(),
  useServerSync: vi.fn(),
  useServerResources: vi.fn(),
  useServerTranscripts: vi.fn(),
}));

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
  expect(accept.mock.calls[1][0]).toEqual(
    cache.peekTranscript("workspace", "a"),
  );
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

it("keeps a retained conversation snapshot renderable while a removal awaits publication", async () => {
  const { createTranscriptCache } = await import("./transcriptCache");
  const { transcriptMessages } = await import("../hooks");
  const cache = createTranscriptCache();
  const batch = {
    id: "batch",
    role: "user" as const,
    text: "input batch",
    inputs: [
      { id: "input", kind: "user", text: "Hello", at: 1 },
      { id: "result", kind: "child_result", text: "Worker finished", at: 2 },
    ],
  };
  const reply = { id: "reply", role: "assistant" as const, text: "Answer" };
  cache.cacheTranscriptValue("workspace", "chat", { items: [batch, reply] }, 1);
  const accept = vi.fn();
  const stop = cache.subscribeTranscript("workspace", "chat", accept);
  // useMessages retains this payload in page.latest until the next publication.
  const retained = accept.mock.calls[0][0];
  const rendered = transcriptMessages(retained.payload.items, "chat");
  expect(rendered.map(({ id, role, text }) => ({ id, role, text }))).toEqual([
    { id: "chat:input", role: "user", text: "Hello" },
    { id: "chat:result", role: "tool", text: "Worker finished" },
    { id: "reply", role: "assistant", text: "Answer" },
  ]);

  cache.patchTranscriptValue("workspace", "chat", {}, 2, [], ["batch"]);

  expect(accept).toHaveBeenCalledOnce();
  // Before the fix, the retained array contains a null tombstone and this throws
  // "Cannot read properties of null (reading 'inputs')" in the real normalizer.
  expect(transcriptMessages(retained.payload.items, "chat")).toEqual(rendered);
  expect(retained.payload.items).toEqual([batch, reply]);
  const current = cache.peekTranscript("workspace", "chat")!;
  expect(current.payload.items).toEqual([reply]);
  expect(current.payload.items[0]).toBe(reply);
  expect(transcriptMessages(current.payload.items, "chat")).toEqual([
    rendered[2],
  ]);
  vi.advanceTimersByTime(50);
  expect(accept).toHaveBeenCalledTimes(2);
  expect(accept.mock.calls[1][0]).toBe(current);
  stop();
});

it.each(["append", "replace", "remove", "reorder"] as const)(
  "preserves published snapshot contents and unchanged item identity on %s",
  async (operation) => {
    const { createTranscriptCache } = await import("./transcriptCache");
    const cache = createTranscriptCache();
    const first = { id: "first", text: "Original" };
    const kept = { id: "kept", text: "Unchanged" };
    const added = { id: "added", text: "New" };
    const replacement = { id: "first", text: "Updated" };
    const items = [first, kept];
    cache.cacheTranscriptValue("workspace", "chat", { items }, 1);
    const retained = cache.peekTranscript("workspace", "chat")!;

    cache.patchTranscriptValue(
      "workspace",
      "chat",
      {},
      2,
      operation === "append"
        ? [added]
        : operation === "replace"
          ? [replacement]
          : [],
      operation === "remove" ? ["first"] : [],
      operation === "reorder" ? ["kept", "first"] : undefined,
    );

    expect(retained.payload.items).toBe(items);
    expect(items).toEqual([first, kept]);
    expect(retained.seq).toBe(1);
    const current = cache.peekTranscript("workspace", "chat")!;
    const expected = {
      append: [first, kept, added],
      replace: [replacement, kept],
      remove: [kept],
      reorder: [kept, first],
    }[operation];
    expect(current.seq).toBe(2);
    expect(current.payload.items).toEqual(expected);
    for (const [index, item] of expected.entries())
      expect(current.payload.items[index]).toBe(item);
  },
);
