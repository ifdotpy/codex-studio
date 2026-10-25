import { afterEach, expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  cursor: 0,
  cells: [] as any[],
  effects: [] as Array<() => void>,
  data: new Map<string, unknown>(),
  get: vi.fn(async () => ({})),
  storage: {},
  writes: [] as string[],
  nullRefs: 0,
}));
vi.mock("react", async (original) => {
  const react = await original<typeof import("react")>();
  const memo = (factory: () => unknown, deps: unknown[]) => {
    const index = h.cursor++;
    const old = h.cells[index];
    if (!old || deps.some((value, i) => value !== old.deps[i]))
      h.cells[index] = { deps, value: factory() };
    return h.cells[index].value;
  };
  return {
    ...react,
    useState: (initial: unknown) => {
      const index = h.cursor++;
      h.cells[index] ??= {
        value: typeof initial === "function" ? initial() : initial,
      };
      return [
        h.cells[index].value,
        (value: any) => {
          h.cells[index].value =
            typeof value === "function" ? value(h.cells[index].value) : value;
        },
      ];
    },
    useRef: (current: unknown) => {
      const index = h.cursor++;
      h.cells[index] ??= {
        current:
          current === null && h.nullRefs++ === 0 ? { clientWidth: 0 } : current,
      };
      return h.cells[index];
    },
    useMemo: memo,
    useCallback: (callback: unknown, deps: unknown[]) =>
      memo(() => callback, deps),
    useEffect: (effect: () => any, deps: unknown[]) => {
      const index = h.cursor++;
      const old = h.cells[index];
      if (!old || !deps || deps.some((value, i) => value !== old.deps[i])) {
        old?.cleanup?.();
        h.cells[index] = { deps };
        h.effects.push(() => {
          h.cells[index].cleanup = effect();
        });
      }
    },
    useLayoutEffect: () => {},
  };
});
vi.mock("../sync/subscriptions", () => ({
  useSubscription: (ref: any) => ({
    data: ref ? h.data.get(ref.kind) : undefined,
    status: "live",
    error: null,
  }),
}));
vi.mock("../sync/client", () => ({}));
vi.mock("../sync/transcriptCache", () => ({}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({
    get: h.get,
    syncGet: h.get,
    saved: (_key: string, value: unknown) => value,
  }),
  useServerStorage: () => h.storage,
  useServerServices: () => ({ owner: { id: "local" } }),
  useServerSync: () => ({}),
  useServerResources: () => ({}),
  useServerTranscripts: () => ({ peekTranscript: () => undefined }),
}));
vi.mock("@xterm/xterm", () => ({
  Terminal: class {
    options = {};
    write(value: string) {
      h.writes.push(value);
    }
    loadAddon() {}
    open() {}
    onData() {}
    reset() {}
    dispose() {}
  },
}));
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class {
    fit() {}
  },
}));
import BackgroundTasks from "./shell/BackgroundTasks";
import Tasks from "./shell/Tasks";
import { UserMessage } from "./shell/messages/UserMessages";
import { ShellView } from "./TerminalDock";
import { useMessages } from "../hooks";
const snapshot = () =>
  ({
    stateDir: "fixture",
    threads: [{ id: "lead", epoch: 1 }],
    runtime: { tasks: [], monitors: [], requests: [], complaints: [] },
  }) as any;
function render<T>(fn: () => T) {
  h.cursor = 0;
  const result = fn();
  for (const effect of h.effects.splice(0)) effect();
  return result;
}
afterEach(() => {
  for (const cell of h.cells) cell.cleanup?.();
  h.cells = [];
  h.effects = [];
  h.data.clear();
  h.get.mockClear();
  h.writes = [];
  h.nullRefs = 0;
  vi.unstubAllGlobals();
});
it("process owner/task events consume process records without GET /api/processes", () => {
  h.nullRefs = 100;
  h.data.set("processes", { processes: [] });
  const props = {
    opened: true,
    close: vi.fn(),
    data: snapshot(),
    leadId: "lead",
    openAgent: vi.fn(),
    refresh: vi.fn(),
    notify: vi.fn(),
  };
  render(() => BackgroundTasks(props));
  h.data.set("processes", { processes: [] });
  render(() =>
    BackgroundTasks({
      ...props,
      data: {
        ...snapshot(),
        threads: [{ id: "lead", epoch: 2 }],
        runtime: { ...snapshot().runtime, tasks: [] },
      },
    }),
  );
  expect(h.get).not.toHaveBeenCalled();
});
it("work revision changes consume the stream without GET /api/work", () => {
  h.nullRefs = 100;
  h.data.set("work", { items: [], rootId: "lead" });
  render(() => Tasks({ agentId: "lead", revision: 1 }));
  h.data.set("work", { items: [], rootId: "lead" });
  render(() => Tasks({ agentId: "lead", revision: 2 }));
  expect(h.get).not.toHaveBeenCalled();
});
it("complaint version changes consume streamed detail without GET /api/complaint", () => {
  h.nullRefs = 100;
  const props = {
    message: { id: "message", version: 1 },
    data: snapshot(),
    focused: false,
    requests: new Map(),
    pendingKey: "replies",
    refresh: vi.fn(),
    notify: vi.fn(),
  } as any;
  h.data.set("complaint", {
    id: "message",
    version: 1,
    text: "First",
    responses: [],
  });
  render(() => UserMessage(props));
  h.data.set("complaint", {
    id: "message",
    version: 2,
    text: "Updated",
    responses: [],
  });
  render(() =>
    UserMessage({ ...props, message: { ...props.message, version: 2 } }),
  );
  expect(h.get).not.toHaveBeenCalled();
});
it.each(["agent", "room"] as const)(
  "%s records update a nonmanaged conversation without a resource GET",
  (kind) => {
    h.nullRefs = 100;
    h.data.set(
      kind === "agent" ? "transcript" : "room",
      kind === "agent"
        ? { items: [{ id: "new", role: "assistant", text: "Streamed" }] }
        : {
            messages: [
              {
                id: "new",
                seq: 1,
                text: "Streamed",
                created: 1,
                sender: "lead",
              },
            ],
            room: { id: "team" },
          },
    );
    const read = () =>
      useMessages("team", kind, false, "workspace", "workspace");
    render(read);
    expect(render(read).items.map((item) => item.text)).toEqual(["Streamed"]);
    expect(h.get).not.toHaveBeenCalled();
  },
);
it("terminal hints apply Unicode appends without GET /api/terminals/output", async () => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  const shell = {
    id: "shell",
    agent: "lead",
    title: "Shell",
    cwd: "/",
    status: "running",
    created: 1,
  } as const;
  h.data.set("terminal-output", {
    text: "a🙂",
    offset: 2,
    status: "running",
    truncated: false,
  });
  const read = () => ShellView({ shell, notify: vi.fn() });
  render(read);
  await vi.waitFor(() => expect(h.writes).toEqual(["a🙂"]));
  h.data.set("terminal-output", {
    text: "a🙂中",
    offset: 3,
    status: "running",
    truncated: false,
  });
  render(read);
  expect(h.writes).toEqual(["a🙂", "中"]);
  expect(h.get).not.toHaveBeenCalled();
});
