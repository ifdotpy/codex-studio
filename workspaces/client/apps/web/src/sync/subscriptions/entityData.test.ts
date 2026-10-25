import { expect, it, vi } from "vitest";
import { createEntityDataObserver } from "./entityData";
import type { createSyncClient } from "../client";
vi.mock("../client", () => ({ syncDatabase: vi.fn() }));

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
