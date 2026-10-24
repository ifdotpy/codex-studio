import { afterEach, expect, it, vi } from "vitest";
import type { SidebarServices } from "./sidebar/services";
const h = vi.hoisted(() => ({ effects: [] as Array<() => unknown> }));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    vi.fn(),
  ],
  useRef: (value: unknown) => ({ current: value }),
  useEffect: (effect: () => unknown) => {
    h.effects.push(effect);
  },
}));
import { useSidebarOrder } from "./useSidebarOrder";
afterEach(() => {
  h.effects = [];
});

it.each(["empty", "legacy", "pending"] as const)(
  "sidebar order %s preserves intentional writes without creating an empty migration",
  async (scenario) => {
    const groups = scenario === "legacy" ? { projects: ["one"] } : {};
    const pending =
      scenario === "pending"
        ? {
            body: {
              action: "reorder",
              request_id: "saved-request",
              expected_revision: 0,
              groups: {},
              migration: true,
            },
          }
        : null;
    const post = vi.fn(async (_path: string, _body: unknown) => ({
      revision: 1,
      groups,
    }));
    const backend = { ownerId: "local", post };
    const services = {
      cache: {
        ownerId: "local",
        saved: (key: string, fallback: unknown) =>
          key === "order"
            ? groups
            : key === "order:pending"
              ? pending
              : fallback,
        save: vi.fn(),
        storage: { setItem: vi.fn(), removeItem: vi.fn() },
      },
      owner: () => backend,
      byOwner: () => backend,
      available: () => true,
    } as unknown as SidebarServices;
    useSidebarOrder(
      "order",
      { revision: 0, groups: null },
      undefined,
      undefined,
      services,
      "session",
    );
    for (const effect of h.effects.splice(0)) effect();
    await Promise.resolve();
    await Promise.resolve();
    if (scenario === "empty") expect(post).not.toHaveBeenCalled();
    else {
      expect(post).toHaveBeenCalledTimes(1);
      expect(post.mock.calls[0][1]).toEqual(
        pending?.body ?? expect.objectContaining({ groups, migration: true }),
      );
    }
  },
);
