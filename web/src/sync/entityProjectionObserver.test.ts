import type { RxCollection } from "rxdb";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { observeEntityProjection } from "./entityProjectionObserver";
import {
  applyEntityChanges,
  emptyEntityProjection,
  type EntityRow,
} from "./entityProjection";

const row = (id: string, seq: number, name = id): EntityRow => ({
  id: `entity:agent:${id}`,
  seq,
  payload: JSON.stringify({ collection: "agent", id, value: { id, name } }),
});
const marker = (payload = "ready", seq = 1): EntityRow => ({
  id: "state:entities:ready",
  payload,
  seq,
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => (resolve = done));
  return { promise, resolve };
}
function source(initial: EntityRow[], readyMarker = marker()) {
  const listeners = new Set<(event: { documentData: EntityRow }) => void>();
  const current = new Map(initial.map((value) => [value.id, value]));
  current.set(readyMarker.id, readyMarker);
  const toJSON = vi.fn((value: EntityRow) => value);
  const query = vi.fn(async () =>
    [...current.values()]
      .filter((value) => value.id.startsWith("entity:") && !value._deleted)
      .map((value) => ({
        toJSON: () => toJSON(value),
      })),
  );
  const collection = {
    $: {
      subscribe(listener: (event: { documentData: EntityRow }) => void) {
        listeners.add(listener);
        return { unsubscribe: () => listeners.delete(listener) };
      },
    },
    storageInstance: {
      findDocumentsById: vi.fn(async (ids: string[]) =>
        ids.flatMap((id) => (current.has(id) ? [current.get(id)!] : [])),
      ),
    },
    find: vi.fn(() => ({ exec: query })),
  };
  return {
    collection: collection as unknown as RxCollection<EntityRow>,
    query,
    read: collection.storageInstance.findDocumentsById,
    toJSON,
    rows(next: EntityRow[]) {
      for (const id of current.keys())
        if (id.startsWith("entity:")) current.delete(id);
      for (const value of next) current.set(value.id, value);
    },
    emit(value: EntityRow, persist = true) {
      if (persist) current.set(value.id, value);
      for (const listener of listeners) listener({ documentData: value });
    },
  };
}
async function settle() {
  for (let index = 0; index < 5; index++) await Promise.resolve();
}
beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal("document", { hidden: true });
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it.each([2000, 20000])(
  "reads %i entities once and publishes 100 changes once",
  async (count) => {
    const input = source(
      Array.from({ length: count }, (_, index) => row(`${index}`, index + 1)),
    );
    const accept = vi.fn();
    const stop = await observeEntityProjection(
      input.collection,
      accept,
      vi.fn(),
    );
    await settle();
    await vi.advanceTimersByTimeAsync(50);
    const initial = accept.mock.calls[0][0];
    expect(initial.threads).toHaveLength(count);
    expect(input.toJSON).toHaveBeenCalledTimes(count);
    const initialReads = input.read.mock.calls.length;
    for (let index = 0; index < 100; index++)
      input.emit(row("0", count + index + 1, `update-${index}`));
    expect(accept).toHaveBeenCalledOnce();
    await vi.advanceTimersByTimeAsync(50);
    expect(accept).toHaveBeenCalledTimes(2);
    const changed = accept.mock.calls[1][0];
    expect(changed.threads[0].name).toBe("update-99");
    expect(changed.threads[1]).toBe(initial.threads[1]);
    expect(changed.runtime.projects).toBe(initial.runtime.projects);
    expect(input.toJSON).toHaveBeenCalledTimes(count);
    expect(input.query).toHaveBeenCalledOnce();
    expect(input.read).toHaveBeenCalledTimes(initialReads + 1);
    expect(input.read).toHaveBeenLastCalledWith(
      ["state:entities:ready", "entity:agent:0"],
      true,
    );
    stop();
  },
);

it("applies events that arrive during the baseline read, including deletes", async () => {
  const input = source([row("a", 1), row("deleted", 2)]);
  const baseline = deferred<Array<{ toJSON(): EntityRow }>>();
  input.query.mockImplementationOnce(() => baseline.promise);
  const accept = vi.fn();
  const stop = await observeEntityProjection(input.collection, accept, vi.fn());
  input.emit(row("a", 3, "new"));
  input.emit({ ...row("deleted", 4), _deleted: true });
  input.emit(row("b", 5));
  input.emit(row("a", 2, "stale"), false);
  baseline.resolve(
    [row("a", 1), row("deleted", 2)].map((value) => ({ toJSON: () => value })),
  );
  await settle();
  await vi.advanceTimersByTimeAsync(50);
  expect(accept.mock.calls[0][0].threads).toEqual([
    { id: "a", name: "new" },
    { id: "b", name: "b" },
  ]);
  stop();
});

it("discards pending publication and old baseline reads when a reset lowers sequences", async () => {
  const input = source([row("a", 100), row("removed", 101)]);
  const oldBaseline = deferred<Array<{ toJSON(): EntityRow }>>();
  input.query.mockImplementationOnce(() => oldBaseline.promise);
  const accept = vi.fn();
  const stop = await observeEntityProjection(input.collection, accept, vi.fn());
  input.emit(marker("resetting", 2));
  input.emit({ ...row("a", 0), _deleted: true });
  input.emit(row("a", 1, "replacement"));
  input.rows([row("a", 1, "replacement")]);
  input.emit(marker("ready", 3));
  await settle();
  oldBaseline.resolve(
    [row("removed", 101)].map((value) => ({ toJSON: () => value })),
  );
  await settle();
  await vi.advanceTimersByTimeAsync(100);
  expect(accept).toHaveBeenCalledOnce();
  expect(accept.mock.calls[0][0].threads).toEqual([
    { id: "a", name: "replacement" },
  ]);

  input.emit(row("a", 2, "never published"));
  input.emit(marker("resetting", 4));
  await vi.advanceTimersByTimeAsync(100);
  expect(accept).toHaveBeenCalledOnce();
  input.rows([row("b", 1)]);
  input.emit(marker("ready", 5));
  await settle();
  await vi.advanceTimersByTimeAsync(100);
  expect(accept.mock.calls[1][0].threads).toEqual([{ id: "b", name: "b" }]);
  input.emit(marker("resetting", 2), false);
  input.emit(row("b", 2, "after stale marker"));
  await vi.advanceTimersByTimeAsync(50);
  expect(accept.mock.calls[2][0].threads).toEqual([
    { id: "b", name: "after stale marker" },
  ]);
  stop();
});

it("waits for a reset already in progress and cancels publication after disposal", async () => {
  const input = source([row("old", 100)], marker("resetting", 2));
  const accept = vi.fn();
  const stop = await observeEntityProjection(input.collection, accept, vi.fn());
  expect(input.query).not.toHaveBeenCalled();
  input.emit(row("new", 1));
  await vi.advanceTimersByTimeAsync(100);
  expect(accept).not.toHaveBeenCalled();
  input.rows([row("new", 1)]);
  input.emit(marker("ready", 3));
  await settle();
  stop();
  await vi.advanceTimersByTimeAsync(100);
  expect(accept).not.toHaveBeenCalled();
});

it("does not scan retained IDs for one explicit change or tombstone", () => {
  const state = emptyEntityProjection();
  applyEntityChanges(
    state,
    Array.from({ length: 20000 }, (_, index) => row(`${index}`, index + 1)),
    true,
  );
  const get = vi.spyOn(state.rows, "get");
  const iterate = vi.spyOn(state.rows, Symbol.iterator);
  const invalidKeys = vi.spyOn(state.invalidReportedSeq, "keys");
  applyEntityChanges(state, [row("0", 20001, "new")], false);
  applyEntityChanges(state, [{ ...row("1", 20002), _deleted: true }], false);
  const snapshot = applyEntityChanges(state, [], true);
  expect(get).toHaveBeenCalledTimes(2);
  expect(iterate).not.toHaveBeenCalled();
  expect(invalidKeys).not.toHaveBeenCalled();
  expect(snapshot?.threads).toHaveLength(19999);
  expect(snapshot?.threads[0].name).toBe("new");
});

it("retains a local RxDB delete that keeps the same server sequence", () => {
  const state = emptyEntityProjection();
  applyEntityChanges(state, [row("a", 1)], true);
  const removed = applyEntityChanges(
    state,
    [{ ...row("a", 1), _deleted: true }],
    true,
  );
  expect(removed?.threads).toEqual([]);
  expect(applyEntityChanges(state, [row("a", 1)], true)).toBe(removed);
});

it("retries a failed baseline without losing buffered changes", async () => {
  const input = source([row("a", 1)]);
  input.query.mockRejectedValueOnce(new TypeError("temporary read failure"));
  const accept = vi.fn();
  const fail = vi.fn();
  const stop = await observeEntityProjection(input.collection, accept, fail);
  await settle();
  expect(fail).toHaveBeenCalledOnce();
  input.emit(row("a", 2, "new"));
  await vi.advanceTimersByTimeAsync(1050);
  expect(accept.mock.calls[0][0].threads).toEqual([{ id: "a", name: "new" }]);
  expect(input.query).toHaveBeenCalledTimes(2);
  stop();
});

it("reads durable rows when old entity and marker events arrive after a reset", async () => {
  const input = source([row("a", 100), row("removed", 101)]);
  const accept = vi.fn();
  const stop = await observeEntityProjection(input.collection, accept, vi.fn());
  await settle();
  await vi.advanceTimersByTimeAsync(50);
  input.rows([row("a", 1, "replacement")]);
  // The reset finishes before the next frame can read its intermediate marker.
  input.emit(marker("ready", 3));
  await vi.advanceTimersByTimeAsync(100);
  expect(accept.mock.calls[1][0].threads).toEqual([
    { id: "a", name: "replacement" },
  ]);
  input.emit(row("a", 100, "old epoch"), false);
  input.emit(row("removed", 101), false);
  input.emit(marker("resetting", 2), false);
  await vi.advanceTimersByTimeAsync(50);
  expect(accept).toHaveBeenCalledTimes(2);
  expect(input.query).toHaveBeenCalledTimes(2);
  stop();
});

it("keeps changes that arrive during a storage read for the next frame", async () => {
  const input = source([row("a", 1)]);
  const accept = vi.fn();
  const stop = await observeEntityProjection(input.collection, accept, vi.fn());
  await settle();
  await vi.advanceTimersByTimeAsync(50);
  const read = input.read.getMockImplementation()!;
  const inFlight = deferred<EntityRow[]>();
  let captured: EntityRow[] = [];
  input.read.mockImplementationOnce(async (ids) => {
    captured = await read(ids);
    return inFlight.promise;
  });
  input.emit(row("a", 2, "second"));
  await vi.advanceTimersByTimeAsync(50);
  input.emit(row("a", 3, "third"));
  await vi.advanceTimersByTimeAsync(50);
  expect(accept).toHaveBeenCalledOnce();
  inFlight.resolve(captured);
  await settle();
  await vi.advanceTimersByTimeAsync(50);
  expect(accept.mock.calls.at(-1)?.[0].threads).toEqual([
    { id: "a", name: "third" },
  ]);
  stop();
});

it("discards a storage result when a reset marker arrives during the read", async () => {
  const input = source([row("a", 100)]);
  const accept = vi.fn();
  const stop = await observeEntityProjection(input.collection, accept, vi.fn());
  await settle();
  await vi.advanceTimersByTimeAsync(50);
  const read = input.read.getMockImplementation()!;
  const inFlight = deferred<EntityRow[]>();
  let captured: EntityRow[] = [];
  input.read.mockImplementationOnce(async (ids) => {
    captured = await read(ids);
    return inFlight.promise;
  });
  input.emit(row("a", 101, "never published"));
  await vi.advanceTimersByTimeAsync(50);
  input.emit(marker("resetting", 2));
  inFlight.resolve(captured);
  await settle();
  await vi.advanceTimersByTimeAsync(50);
  expect(accept).toHaveBeenCalledOnce();
  input.rows([row("b", 1)]);
  input.emit(marker("ready", 3));
  await vi.advanceTimersByTimeAsync(100);
  expect(accept.mock.calls[1][0].threads).toEqual([{ id: "b", name: "b" }]);
  stop();
});
