import { expect, it, vi } from "vitest";
import { createEntityDataObserver } from "./entityData";
import type { createSyncClient } from "../client";
vi.mock("../client", () => ({ syncDatabase: vi.fn() }));

it("selects one visible agent detail before decoding and releases its observer", async () => {
  let emit!: (rows: { id: string; toJSON: () => unknown }[]) => void;
  const unsubscribe = vi.fn();
  const find = vi.fn((query?: { selector?: { id?: string } }) => ({
    $: {
      subscribe: ({
        next,
      }: {
        next: (rows: { toJSON: () => unknown }[]) => void;
      }) => {
        emit = (rows) =>
          next(
            query?.selector?.id
              ? rows.filter((row) => row.id === query.selector!.id)
              : rows,
          );
        return { unsubscribe };
      },
    },
  }));
  const observe = createEntityDataObserver((async () => ({
    db: { projections: { find } },
  })) as unknown as ReturnType<typeof createSyncClient>["syncDatabase"]);
  const accept = vi.fn();
  const stop = observe(
    { kind: "agentDetail", agentId: "visible" },
    accept,
    vi.fn(),
  );
  await vi.waitFor(() => expect(emit).toBeTypeOf("function"));
  const visible = {
    id: "entity:agentDetail:visible",
    toJSON: vi.fn(() => ({
      id: "entity:agentDetail:visible",
      seq: 1,
      entity: {
        collection: "agentDetail",
        id: "visible",
        value: { id: "visible", overview: { task: "Current work" } },
      },
    })),
  };
  const unrelated = {
    id: "entity:workResult:other:0",
    toJSON: vi.fn(() => ({ id: "entity:workResult:other:0" })),
  };
  emit([visible, unrelated]);
  expect(find).toHaveBeenCalledExactlyOnceWith({
    selector: { id: "entity:agentDetail:visible" },
  });
  expect(visible.toJSON).toHaveBeenCalledTimes(1);
  expect(unrelated.toJSON).not.toHaveBeenCalled();
  expect(accept).toHaveBeenCalledTimes(1);
  emit([visible, unrelated]);
  expect(accept).toHaveBeenCalledTimes(1);
  stop();
  expect(unsubscribe).toHaveBeenCalledOnce();
});

it("ignores offscreen workspace changes and preserves unchanged typed row identities", async () => {
  let emit!: (rows: { toJSON: () => unknown }[]) => void;
  const observe = createEntityDataObserver((async () => ({
    db: {
      projections: {
        find: () => ({
          $: {
            subscribe: ({ next }: { next: typeof emit }) => {
              emit = next;
              return { unsubscribe() {} };
            },
          },
        }),
      },
    },
  })) as unknown as ReturnType<typeof createSyncClient>["syncDatabase"]);
  const accept = vi.fn();
  const stop = observe({ kind: "work", agentId: "lead" }, accept, vi.fn());
  await vi.waitFor(() => expect(emit).toBeTypeOf("function"));
  const row = (id: string, rootId: string, seq: number) => ({
    id: `entity:workDetail:${id}`,
    seq,
    entity: {
      collection: "workDetail",
      id,
      value: {
        id,
        rootId,
        title: id,
        status: "running",
        version: 1,
        created: 1,
        updated: 1,
      },
    },
  });
  const first = row("first", "lead", 1),
    second = row("second", "lead", 2),
    hidden = row("hidden", "other", 3);
  const send = (values: unknown[]) =>
    emit(values.map((value) => ({ toJSON: () => structuredClone(value) })));
  send([first, second, hidden]);
  const initial = accept.mock.calls.at(-1)![0];
  expect(initial.items).toHaveLength(2);
  send([first, second, { ...hidden, seq: 4 }]);
  expect(accept).toHaveBeenCalledTimes(1);
  send([
    {
      ...first,
      seq: 5,
      entity: {
        ...first.entity,
        value: { ...first.entity.value, status: "review" },
      },
    },
    second,
    hidden,
  ]);
  expect(accept).toHaveBeenCalledTimes(2);
  expect(accept.mock.calls.at(-1)![0].items[1]).toBe(initial.items[1]);
  stop();
});

it("clears a retained detail after its authoritative local row is removed", async () => {
  let emit!: (rows: { toJSON: () => unknown }[]) => void;
  const unsubscribe = vi.fn();
  const database = vi.fn(async () => ({
    db: {
      projections: {
        find: () => ({
          $: {
            subscribe: ({ next }: { next: typeof emit }) => {
              emit = next;
              return { unsubscribe };
            },
          },
        }),
      },
    },
  }));
  const observe = createEntityDataObserver(
    database as unknown as ReturnType<typeof createSyncClient>["syncDatabase"],
  );
  const accept = vi.fn();
  const stop = observe(
    { kind: "agentDetail", agentId: "visible" },
    accept,
    vi.fn(),
  );
  await vi.waitFor(() => expect(emit).toBeTypeOf("function"));
  const value = { id: "visible", name: "Current detail" };
  emit([
    {
      toJSON: () => ({
        id: "entity:agentDetail:visible",
        entity: { collection: "agentDetail", id: "visible", value },
      }),
    },
  ]);
  expect(accept).toHaveBeenLastCalledWith(value);
  emit([]);
  expect(accept).toHaveBeenLastCalledWith(undefined);
  stop();
  expect(unsubscribe).toHaveBeenCalledOnce();
});
