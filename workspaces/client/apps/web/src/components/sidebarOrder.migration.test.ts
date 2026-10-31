import { required } from "./componentTestFixtures";
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

it.each(["empty", "empty-group", "legacy", "pending"] as const)(
  "sidebar order %s preserves intentional writes without creating an empty migration",
  async (scenario) => {
    const groups =
      scenario === "legacy"
        ? { projects: ["one"] }
        : scenario === "empty-group"
          ? { projects: [] }
          : {};
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
          key === "codex-sidebar-order:order"
            ? groups
            : key === "codex-sidebar-order:order:pending"
              ? pending
              : fallback,
        save: vi.fn(),
        storage: {
          getItem: () => (pending ? JSON.stringify(pending) : null),
          setItem: vi.fn(),
          removeItem: vi.fn(),
        },
      },
      owner: () => backend,
      byOwner: () => backend,
      available: () => true,
    } as unknown as SidebarServices;
    useSidebarOrder(
      "codex-sidebar-order:order",
      { revision: 0, groups: null },
      undefined,
      undefined,
      services,
      "session",
    );
    for (const effect of h.effects.splice(0)) effect();
    await Promise.resolve();
    await Promise.resolve();
    if (scenario !== "pending") expect(post).not.toHaveBeenCalled();
    else {
      expect(post).toHaveBeenCalledTimes(1);
      expect(required(required(post.mock.calls[0])[1])).toEqual(
        pending?.body ?? expect.objectContaining({ groups, migration: true }),
      );
    }
  },
);

it("allows an explicit reorder when the server has no previous order", async () => {
  const post = vi.fn(async (_path: string, _body: unknown) => ({
    revision: 1,
    groups: { projects: ["two", "one"] },
  }));
  const backend = { ownerId: "local", post };
  const services = {
    cache: {
      ownerId: "local",
      storage: { getItem: () => null, setItem: vi.fn(), removeItem: vi.fn() },
    },
    owner: () => backend,
    byOwner: () => backend,
    available: () => true,
  } as unknown as SidebarServices;
  const sorting = useSidebarOrder(
    "codex-sidebar-order:scope",
    { revision: 1, groups: null },
    undefined,
    undefined,
    services,
    "session",
  );
  for (const effect of h.effects.splice(0)) effect();
  const first = sorting.bindings("projects", "one", ["one", "two"]);
  const second = sorting.bindings("projects", "two", ["one", "two"]);
  first.onDragStart({
    stopPropagation() {},
    dataTransfer: { effectAllowed: "", setData() {} },
  } as never);
  second.onDrop({
    preventDefault() {},
    stopPropagation() {},
    clientY: 100,
    currentTarget: { getBoundingClientRect: () => ({ top: 0, height: 100 }) },
  } as never);
  await Promise.resolve();
  expect(post).toHaveBeenCalledOnce();
  expect(required(required(post.mock.calls[0])[1])).toEqual(
    expect.objectContaining({
      action: "reorder",
      groups: { projects: ["two", "one"] },
    }),
  );
});
