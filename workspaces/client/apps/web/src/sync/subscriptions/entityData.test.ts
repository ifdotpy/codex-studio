import { expect, it, vi } from "vitest";
import { createEntityDataObserver } from "./entityData";
import type { createSyncClient } from "../client";
vi.mock("../client", () => ({ syncDatabase: vi.fn() }));

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
