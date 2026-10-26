import { expect, it, vi } from "vitest";
import { DraftJournal } from "./draftJournal";
import { NativeDraftState, type DraftView } from "./draftState";

function memory(): Storage {
  const data = new Map<string, string>();
  return {
    get length() {
      return data.size;
    },
    key: (index) => [...data.keys()][index] ?? null,
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => {
      data.set(key, value);
    },
    removeItem: (key) => {
      data.delete(key);
    },
    clear: () => data.clear(),
  };
}
const view = (text: string, revision: number): DraftView => ({
  drafts: { room: text },
  conflicts: [],
  workspaceId: "workspace",
  revision,
});

it("restores exact uncertain requests and later text after restart", async () => {
  const storage = memory();
  const lost = vi.fn().mockRejectedValue(new Error("reply lost"));
  const first = new NativeDraftState(
    "workspace",
    lost,
    new DraftJournal(storage, "workspace"),
  );
  first.edit({ room: "committed, reply lost" });
  await Promise.resolve();
  first.edit({ room: "later Ж🦀" });
  expect(storage.length).toBe(2);
  const invoke = vi
    .fn()
    .mockResolvedValueOnce(view("committed, reply lost", 1))
    .mockResolvedValueOnce(view("later Ж🦀", 2));
  const restarted = new NativeDraftState(
    "workspace",
    invoke,
    new DraftJournal(storage, "workspace"),
  );
  restarted.accept(view("older remote", 0));
  expect(restarted.getSnapshot().drafts.room).toBe("later Ж🦀");
  expect(restarted.getSnapshot().pending).toBe(true);
  await restarted.retry();
  expect(invoke.mock.calls[0]).toEqual(lost.mock.calls[0]);
  expect(invoke.mock.calls[1]?.[2]).not.toBe(invoke.mock.calls[0]?.[2]);
  expect(storage.length).toBe(0);
  expect(restarted.getSnapshot()).toMatchObject({
    pending: false,
    drafts: { room: "later Ж🦀" },
  });
});

it("keeps a failed journal write visible and blocks IPC until recovery", async () => {
  const storage = memory();
  const write = storage.setItem;
  storage.setItem = () => {
    throw new Error("quota");
  };
  const invoke = vi.fn().mockResolvedValue(view("keep", 1));
  const state = new NativeDraftState(
    "workspace",
    invoke,
    new DraftJournal(storage, "workspace"),
  );
  state.edit({ room: "keep" });
  expect(invoke).not.toHaveBeenCalled();
  expect(state.getSnapshot()).toMatchObject({
    localError: "quota",
    drafts: { room: "keep" },
  });
  await expect(state.waitCommitted()).rejects.toThrow("quota");
  storage.setItem = write;
  await state.retry();
  expect(invoke).toHaveBeenCalledOnce();
  expect(storage.length).toBe(0);
});

it("retains an acknowledged request when journal cleanup fails", async () => {
  const storage = memory();
  storage.removeItem = () => {
    throw new Error("storage unavailable");
  };
  const invoke = vi.fn().mockResolvedValue(view("keep", 1));
  const state = new NativeDraftState(
    "workspace",
    invoke,
    new DraftJournal(storage, "workspace"),
  );
  state.edit({ room: "keep" });
  await Promise.resolve();
  expect(state.getSnapshot().localError).toBe("storage unavailable");
  const restarted = new NativeDraftState(
    "workspace",
    invoke,
    new DraftJournal(storage, "workspace"),
  );
  await restarted.retry();
  expect(invoke.mock.calls[1]).toEqual(invoke.mock.calls[0]);
  expect(storage.length).toBe(1);
});

it("isolates workspaces and refuses a corrupt recovery record", () => {
  const storage = memory();
  const a = new DraftJournal(storage, "a");
  a.save({ id: "request", changes: { room: "a" } });
  expect(new DraftJournal(storage, "b").load()).toEqual([]);
  expect(() => a.save({ id: "request", changes: { room: "changed" } })).toThrow(
    "different content",
  );
  storage.setItem(storage.key(0)!, "{}");
  expect(() => a.load()).toThrow("could not be read");
  expect(storage.length).toBe(1);
});
