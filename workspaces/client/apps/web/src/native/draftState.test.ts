import { required } from "../components/componentTestFixtures.ts";
import { expect, it, vi } from "vitest";
import {
  NativeDraftState,
  nativeDraftQueue,
  type DraftView,
} from "./draftState";
const view = (text: string, revision: number): DraftView => ({
  drafts: { room: text },
  conflicts: [],
  workspaceId: "workspace",
  revision,
  error: "",
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
  expect(required(invoke.mock.calls[1])[1].changes).toEqual({
    room: "latest Ж🦀",
  });
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
  expect(required(invoke.mock.calls[2])[2]).not.toBe(
    required(invoke.mock.calls[0])[2],
  );
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

// These assertions are checked by the normal TypeScript build, not executed.
function snapshotTypeAssertions(state: NativeDraftState) {
  // @ts-expect-error workspace identity is required on the wire
  const missingWorkspace: DraftView = {
    drafts: {},
    conflicts: [],
    revision: 0,
    error: "",
  };
  // @ts-expect-error server revision is required on the wire
  const missingRevision: DraftView = {
    drafts: {},
    conflicts: [],
    workspaceId: "workspace",
    error: "",
  };
  // @ts-expect-error sync error is required on the wire
  const missingError: DraftView = {
    drafts: {},
    conflicts: [],
    workspaceId: "workspace",
    revision: 0,
  };
  // @ts-expect-error published drafts cannot be mutated by a consumer
  state.getSnapshot().drafts.room = "mutated";
  // @ts-expect-error published conflict arrays cannot be mutated by a consumer
  state.getSnapshot().conflicts.push({});
  return [missingWorkspace, missingRevision, missingError];
}
void snapshotTypeAssertions;

function memoryQueue(
  initial: { id: string; changes: Record<string, string> }[] = [],
) {
  const entries = new Map(initial.map((edit) => [edit.id, edit]));
  return {
    entries,
    queue: {
      load: vi.fn(async () => [...entries.values()]),
      save: vi.fn(
        async (edit: { id: string; changes: Record<string, string> }) => {
          entries.set(edit.id, edit);
        },
      ),
    },
  };
}

it("saves each edit in the core queue before its commit", async () => {
  const log: string[] = [];
  const { queue } = memoryQueue();
  queue.save.mockImplementation(async (edit) => {
    log.push(`queue:${edit.id}`);
  });
  const invoke = vi
    .fn()
    .mockImplementation(
      async (method: string, _params: unknown, id?: string) => {
        log.push(`${method}:${id}`);
        return view("first", 1);
      },
    );
  const state = new NativeDraftState("workspace", invoke, queue);
  state.accept(view("", 0));
  state.edit({ room: "first" });
  await vi.waitFor(() => expect(state.getSnapshot().pending).toBe(false));
  const id = required(queue.save.mock.calls[0])[0].id;
  expect(log).toEqual([`queue:${id}`, `drafts.edit:${id}`]);
  expect(required(queue.save.mock.calls[0])[0].changes).toEqual({
    room: "first",
  });
});

it("restores edits queued before a restart and sends them again", async () => {
  const { queue } = memoryQueue([{ id: "old", changes: { room: "unsent" } }]);
  const invoke = vi.fn().mockResolvedValue(view("unsent", 1));
  const state = new NativeDraftState("workspace", invoke, queue);
  await state.restore();
  expect(state.getSnapshot()).toMatchObject({
    pending: true,
    drafts: { room: "unsent" },
  });
  await state.retry();
  expect(invoke).toHaveBeenCalledWith(
    "drafts.edit",
    { workspace: "workspace", changes: { room: "unsent" } },
    "old",
  );
  expect(state.getSnapshot().pending).toBe(false);
});

it("keeps a failed core queue write visible and retries the same identity", async () => {
  const { queue } = memoryQueue();
  queue.save.mockRejectedValueOnce(new Error("reply lost"));
  const invoke = vi.fn().mockResolvedValue(view("keep", 1));
  const state = new NativeDraftState("workspace", invoke, queue);
  state.accept(view("", 0));
  state.edit({ room: "keep" });
  await vi.waitFor(() =>
    expect(state.getSnapshot().localError).toBe("reply lost"),
  );
  expect(invoke).not.toHaveBeenCalled();
  await state.retry();
  const first = required(queue.save.mock.calls[0])[0];
  expect(queue.save.mock.calls[1]).toEqual([first]);
  expect(invoke).toHaveBeenCalledOnce();
  expect(state.getSnapshot()).toMatchObject({ pending: false, localError: "" });
});

it("stores queued edits in the core under the request identity", async () => {
  const invoke = vi
    .fn()
    .mockResolvedValueOnce({})
    .mockResolvedValueOnce({ edits: [{ id: "a", changes: { room: "x" } }] });
  const queue = nativeDraftQueue("workspace", invoke);
  await queue.save({ id: "a", changes: { room: "x" } });
  expect(invoke).toHaveBeenNthCalledWith(
    1,
    "drafts.queue",
    { workspace: "workspace", changes: { room: "x" } },
    "a",
  );
  expect(await queue.load()).toEqual([{ id: "a", changes: { room: "x" } }]);
  expect(invoke).toHaveBeenNthCalledWith(2, "drafts.queued", {
    workspace: "workspace",
  });
});
