import { afterEach, expect, it, vi } from "vitest";
import type { ReactElement, ReactNode } from "react";
const h = vi.hoisted(() => ({
  cursor: 0,
  cells: [] as any[],
  effects: [] as Array<() => void>,
  data: new Map<string, unknown>(),
  statuses: new Map<string, string>(),
  watchConnection: vi.fn((accept: (status: string) => void) => {
    accept("live");
    return () => {};
  }),
  get: vi.fn(async () => ({})),
  storage: {},
  writes: [] as string[],
  nullRefs: 0,
  refs: [] as any[],
  errors: new Map<string, Error>(),
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
  useSubscription: (ref: any) => {
    if (ref) h.refs.push(ref);
    return {
      data: ref ? h.data.get(ref.kind) : undefined,
      status: ref ? (h.statuses.get(ref.kind) ?? "live") : "stale",
      error: ref ? (h.errors.get(ref.kind) ?? null) : null,
    };
  },
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
  useServerResources: () => ({ watchResourceConnection: h.watchConnection }),
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
function render<T>(fn: () => T, offset = 0) {
  h.cursor = offset;
  const result = fn();
  for (const effect of h.effects.splice(0)) effect();
  return result;
}
afterEach(() => {
  for (const cell of h.cells) cell?.cleanup?.();
  h.cells = [];
  h.effects = [];
  h.data.clear();
  h.statuses.clear();
  h.watchConnection.mockClear();
  h.get.mockClear();
  h.writes = [];
  h.nullRefs = 0;
  h.refs = [];
  h.errors.clear();
  vi.unstubAllGlobals();
});
function elements(node: ReactNode): ReactElement<any>[] {
  if (Array.isArray(node)) return node.flatMap(elements);
  if (node && typeof node === "object" && "props" in node) {
    const element = node as ReactElement<any>;
    return [element, ...elements(element.props.children)];
  }
  return [];
}
const processProps = () => ({
  opened: true,
  close: vi.fn(),
  data: snapshot(),
  leadId: "lead",
  openAgent: vi.fn(),
  refresh: vi.fn(),
  notify: vi.fn(),
});
const commandProcess = () => ({
  id: "command",
  agent: "lead",
  created: 2,
  status: "running",
  source: "codex_task",
  key: "command-key",
  generation: "generation",
  accountKey: "default",
  epoch: 1,
  output: "Streamed command output",
  capabilities: {
    readOutput: true,
    sendInput: true,
    resize: false,
    stop: true,
  },
});
it("an open process panel and its selected detail need only two distinct resource subscriptions", () => {
  h.nullRefs = 100;
  h.data.set("tasks", []);
  h.data.set("processes", { processes: [commandProcess()] });
  const view = render(() => BackgroundTasks(processProps()));
  const detail = elements(view).find(
    (node) => node.props.canStop !== undefined,
  )!;
  render(() => (detail.type as Function)(detail.props), 100);
  expect(h.refs).toEqual([
    { kind: "tasks", agentId: "lead" },
    { kind: "processes", agentId: "lead" },
  ]);
  expect(h.get).not.toHaveBeenCalled();
});
it("process rows use streamed task-only records and authoritative process output without duplicates", () => {
  h.nullRefs = 100;
  const props = processProps();
  props.data.runtime.tasks = [
    { id: "stale", agent: "lead", created: 1, status: "running", kind: "tool" },
  ];
  const process = commandProcess();
  h.data.set("processes", { processes: [process] });
  h.data.set("tasks", [
    { ...process, kind: "command", tail: "Older task output" },
    {
      id: "tool",
      agent: "lead",
      created: 1,
      status: "running",
      kind: "tool",
      name: "webSearch",
    },
    {
      id: "claude",
      agent: "lead",
      created: 1,
      status: "running",
      kind: "tool",
      itemId: "task:background",
    },
  ]);
  const read = () => BackgroundTasks(props);
  const view = render(read);
  const rows = elements(view).filter((node) => node.props.select);
  expect(rows.map((node) => node.props.task.id).sort()).toEqual([
    "claude",
    "command",
    "tool",
  ]);
  expect(
    rows.find((node) => node.props.task.id === "command")!.props.task.tail,
  ).toBe("Streamed command output");
  const detail = elements(view).find(
    (node) => node.props.canStop !== undefined,
  )!;
  const detailView = render(() => (detail.type as Function)(detail.props), 100);
  expect(
    elements(detailView).find((node) =>
      node.props.className?.startsWith("task-output"),
    )!.props.children,
  ).toBe("Streamed command output");
  expect(detail.props.canStop).toBe(true);
  h.data.set("processes", {
    processes: [
      {
        ...process,
        output: "Updated output",
        error: "Streamed error",
        cancelRequested: true,
      },
    ],
  });
  const updated = elements(render(read)).find(
    (node) => node.props.canStop !== undefined,
  )!;
  expect(updated.key).toBe(detail.key);
  expect(updated.props.task).toMatchObject({
    tail: "Updated output",
    error: "Streamed error",
    cancelRequested: true,
  });
  h.data.set("tasks", []);
  expect(
    elements(render(read))
      .filter((node) => node.props.select)
      .map((node) => node.props.task.id),
  ).toEqual(["command"]);
  expect(h.get).not.toHaveBeenCalled();
});
it.each([false, true])(
  "a process panel without visible scope has no resource subscriptions (opened=%s)",
  (opened) => {
    h.nullRefs = 100;
    render(() =>
      BackgroundTasks({
        ...processProps(),
        opened,
        ...(opened ? { leadId: undefined } : {}),
      }),
    );
    expect(h.refs).toEqual([]);
    expect(h.get).not.toHaveBeenCalled();
  },
);
it("task-only stream failures remain visible beside available process rows", () => {
  h.nullRefs = 100;
  h.data.set("processes", { processes: [commandProcess()] });
  h.statuses.set("tasks", "error");
  h.errors.set("tasks", new Error("Task stream unavailable"));
  const nodes = elements(render(() => BackgroundTasks(processProps())));
  expect(
    nodes
      .filter((node) => node.props.role === "alert")
      .flatMap((node) => node.props.children)
      .join(""),
  ).toContain("Task stream unavailable");
  expect(
    nodes.filter((node) => node.props.select).map((node) => node.props.task.id),
  ).toEqual(["command"]);
  expect(h.get).not.toHaveBeenCalled();
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
it("keeps live agent transport after visiting a stale room in the same component", () => {
  h.nullRefs = 100;
  h.data.set("room", { messages: [], room: { id: "team" } });
  h.statuses.set("room", "stale");
  const agent = () =>
    useMessages("lead", "agent", true, "workspace", "workspace");
  const room = () =>
    useMessages("team", "room", true, "workspace", "workspace");
  render(agent);
  expect(render(agent).connection).toBe("live");
  render(room);
  expect(render(room).connection).toBe("");
  render(agent);
  expect(render(agent).connection).toBe("live");
  expect(h.watchConnection).toHaveBeenCalledTimes(2);
  expect(h.get).not.toHaveBeenCalled();
});
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
