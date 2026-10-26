import { assert } from "vitest";
import { expect, it, vi } from "vitest";
import { NativeDraftState, type DraftView } from "./draftState";
const view = (text: string, revision: number): DraftView => ({
  drafts: { room: text },
  conflicts: [],
  workspaceId: "workspace",
  revision,
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
it("keeps newer input pending when an older edit is acknowledged", async () => {
  const first = deferred<DraftView>();
  const second = deferred<DraftView>();
  const invoke = vi
    .fn()
    .mockReturnValueOnce(first.promise)
    .mockReturnValueOnce(second.promise);
  const state = new NativeDraftState("workspace", invoke);
  state.accept(view("", 0));
  state.edit({ room: "first" });
  state.edit({ room: "second" });
  state.edit({ room: "latest Ж🦀" });
  state.accept(view("latest Ж🦀", 2));
  expect(state.getSnapshot().pending).toBe(true);
  first.resolve(view("first", 1));
  await first.promise;
  expect(state.getSnapshot().drafts.room).toBe("latest Ж🦀");
  expect(state.getSnapshot().pending).toBe(true);
  expect(invoke).toHaveBeenCalledTimes(2);
  assert(invoke.mock.calls[1] !== undefined);
  expect(invoke.mock.calls[1][1].changes).toEqual({ room: "latest Ж🦀" });
  second.resolve(view("latest Ж🦀", 3));
  await second.promise;
  expect(state.getSnapshot().pending).toBe(false);
  state.accept(view("stale", 1));
  expect(state.getSnapshot().drafts.room).toBe("latest Ж🦀");
});
it("retains exact uncertain command identity and content until an explicit retry", async () => {
  const first = deferred<DraftView>();
  const invoke = vi
    .fn()
    .mockReturnValueOnce(first.promise)
    .mockResolvedValueOnce(view("first", 1))
    .mockResolvedValueOnce(view("newer", 2));
  const state = new NativeDraftState("workspace", invoke);
  state.edit({ room: "first" });
  first.reject(new Error("response lost"));
  await first.promise.catch(() => {});
  state.edit({ room: "newer" });
  expect(invoke).toHaveBeenCalledTimes(1);
  expect(state.getSnapshot()).toMatchObject({
    pending: true,
    localError: "response lost",
    drafts: { room: "newer" },
  });
  await state.retry();
  expect(invoke.mock.calls[1]).toEqual(invoke.mock.calls[0]);
  assert(invoke.mock.calls[2] !== undefined);
  assert(invoke.mock.calls[0] !== undefined);
  expect(invoke.mock.calls[2][2]).not.toBe(invoke.mock.calls[0][2]);
  expect(state.getSnapshot()).toMatchObject({
    pending: false,
    localError: "",
    drafts: { room: "newer" },
  });
});
it("rejects another workspace without discarding unconfirmed text", async () => {
  const invoke = vi
    .fn()
    .mockResolvedValue({ ...view("changed", 8), workspaceId: "other" });
  const state = new NativeDraftState("workspace", invoke);
  state.edit({ room: "keep" });
  await Promise.resolve();
  expect(state.getSnapshot()).toMatchObject({
    pending: true,
    drafts: { room: "keep" },
  });
  expect(state.getSnapshot().localError).toContain("another workspace");
  expect(() =>
    state.accept({ ...view("wrong", 10), workspaceId: "other" }),
  ).toThrow("workspace changed");
});

it("waits for the latest queued edit before allowing close", async () => {
  const first = deferred<DraftView>();
  const last = deferred<DraftView>();
  const invoke = vi
    .fn()
    .mockReturnValueOnce(first.promise)
    .mockReturnValueOnce(last.promise);
  const state = new NativeDraftState("workspace", invoke);
  state.edit({ room: "first" });
  const closed = vi.fn();
  const ready = state.waitCommitted().then(closed);
  state.edit({ room: "last" });
  first.resolve(view("first", 1));
  await first.promise;
  expect(closed).not.toHaveBeenCalled();
  last.resolve(view("last", 2));
  await ready;
  expect(closed).toHaveBeenCalledOnce();
  expect(state.getSnapshot().drafts.room).toBe("last");
});

it("refuses close after a lost reply without repeating the write", async () => {
  const write = deferred<DraftView>();
  const invoke = vi.fn().mockReturnValue(write.promise);
  const state = new NativeDraftState("workspace", invoke);
  state.edit({ room: "keep" });
  const rejected = expect(state.waitCommitted()).rejects.toThrow("reply lost");
  write.reject(new Error("reply lost"));
  await rejected;
  await expect(state.waitCommitted()).rejects.toThrow("reply lost");
  expect(invoke).toHaveBeenCalledOnce();
  expect(state.getSnapshot().pending).toBe(true);
});

it("a close timeout retains the pending write and allows its later acknowledgement", async () => {
  vi.useFakeTimers();
  try {
    const write = deferred<DraftView>();
    const state = new NativeDraftState(
      "workspace",
      vi.fn().mockReturnValue(write.promise),
    );
    state.edit({ room: "keep" });
    const rejected = expect(state.waitCommitted(100)).rejects.toThrow(
      "not finished saving",
    );
    await vi.advanceTimersByTimeAsync(100);
    await rejected;
    expect(state.getSnapshot().pending).toBe(true);
    write.resolve(view("keep", 1));
    await write.promise;
    await state.waitCommitted();
    expect(state.getSnapshot().pending).toBe(false);
  } finally {
    vi.useRealTimers();
  }
});
