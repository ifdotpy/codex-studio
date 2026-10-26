import { afterEach, expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  cleanups: [] as Array<() => void>,
  writes: [] as unknown[],
  context: Promise.resolve({}),
  accept: undefined as ((value: unknown) => void) | undefined,
  report: undefined as ((error: unknown) => void) | undefined,
  select: vi.fn(async () => {}),
  command: vi.fn(async () => ({ found: true })),
  close: vi.fn(async () => {}),
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useMemo: (factory: () => unknown) => factory(),
  useCallback: (callback: unknown) => callback,
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    (value: unknown) => h.writes.push(value),
  ],
  useEffect: (effect: () => void | (() => void)) => h.effects.push(effect),
  useLayoutEffect: vi.fn(),
}));
vi.mock("./servicesContext", () => ({
  useServerServices: () => ({ context: h.context }),
}));
vi.mock("./messageView", () => ({
  createNativeMessageView: (
    context: unknown,
    accept: typeof h.accept,
    report: typeof h.report,
  ) => {
    expect(context).toBe(h.context);
    h.accept = accept;
    h.report = report;
    return { select: h.select, command: h.command, close: h.close };
  },
}));
import { useMessages } from "./hooks";

afterEach(() => {
  for (const cleanup of h.cleanups.splice(0)) cleanup();
  h.effects = [];
  h.writes = [];
  vi.clearAllMocks();
});
function mount(id: string | null = "lead") {
  const hook = useMessages(id, "agent", true, "/state", "a".repeat(32));
  for (const effect of h.effects.splice(0)) {
    const cleanup = effect();
    if (cleanup) h.cleanups.push(cleanup);
  }
  return hook;
}

it("passes selection and every public navigation action to the Rust view owner", async () => {
  const hook = mount();
  expect(hook.loaded).toBe(false);
  expect(h.select).toHaveBeenCalledExactlyOnceWith({
    id: "lead",
    kind: "agent",
    managed: true,
    enabled: true,
    syncWorkspaceId: "a".repeat(32),
  });
  await hook.older();
  await hook.newer();
  hook.setPageAnchor("message");
  expect(await hook.ensureMessage("message")).toBe(true);
  hook.showLatest();
  await hook.reload();
  expect(h.command.mock.calls).toEqual([
    ["older", {}],
    ["newer", {}],
    ["anchor", { anchor: "message" }],
    ["ensure", { messageId: "message" }],
    ["latest", {}],
    ["refresh", {}],
  ]);
});

it("shows native view values and errors without a browser cache or request", () => {
  mount();
  const value = {
    viewId: "view",
    generation: 1,
    loaded: true,
    items: [{ id: "message" }],
    before: "older",
    after: "newer",
    historyVersion: "version",
    historical: true,
    pageLoading: false,
    liveAgent: null,
    connection: "live",
    notice: "",
  };
  h.accept?.(value);
  expect(h.writes).toContainEqual(expect.objectContaining({ view: value }));
  h.report?.(new Error("Native history unavailable"));
  expect(h.writes).toContainEqual(
    expect.objectContaining({ error: "Native history unavailable" }),
  );
});

it("treats an empty selection as loaded and closes the native view on unmount", async () => {
  expect(mount(null).loaded).toBe(true);
  for (const cleanup of h.cleanups.splice(0)) cleanup();
  await Promise.resolve();
  expect(h.close).toHaveBeenCalledTimes(1);
});
