import { required } from "../componentTestFixtures";
import {
  createElement,
  isValidElement,
  type ReactElement,
  type ReactNode,
} from "react";
import { afterEach, expect, test, vi } from "vitest";
import type { Agent } from "../../types";

const h = vi.hoisted(() => ({
  cells: new Map<string, any[]>(),
  scope: "",
  scoped: "",
  cursor: 0,
  effects: [] as Array<() => unknown>,
  post: vi.fn(),
  refresh: vi.fn(async () => {}),
}));
vi.mock("react", async (load) => ({
  ...(await load<typeof import("react")>()),
  useState: (initial: unknown) => {
    const cells = h.cells.get(h.scope)!;
    const index = h.cursor++;
    cells[index] ??= {
      value: typeof initial === "function" ? initial() : initial,
    };
    return [
      required(cells[index]).value,
      (value: unknown) => {
        required(cells[index]).value =
          typeof value === "function"
            ? value(required(cells[index]).value)
            : value;
      },
    ];
  },
  useRef: (value: unknown) => {
    const cells = h.cells.get(h.scope)!;
    return (cells[h.cursor++] ??= { current: value });
  },
  useMemo: (compute: () => unknown, deps: unknown[]) => {
    const cells = h.cells.get(h.scope)!;
    const index = h.cursor++;
    const previous = cells[index];
    if (!previous || deps.some((value, i) => value !== previous.deps[i]))
      cells[index] = { deps, value: compute() };
    return required(cells[index]).value;
  },
  useEffect: (effect: () => unknown, deps: unknown[]) => {
    const cells = h.cells.get(h.scope)!;
    const index = h.cursor++;
    const previous = cells[index];
    if (
      !previous ||
      !deps ||
      deps.some((value, i) => value !== previous.deps?.[i])
    ) {
      previous?.cleanup?.();
      const cell = { deps, cleanup: undefined as unknown };
      cells[index] = cell;
      h.effects.push(() => {
        cell.cleanup = effect();
      });
    }
  },
  useLayoutEffect: () => {},
  useId: () => "settings-role",
  useCallback: (callback: unknown) => callback,
}));
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerApi: () => ({
    post: h.post,
    saved: (_key: string, fallback: unknown) => fallback,
    save: vi.fn(),
  }),
}));
vi.mock("../models/modelWhitelist", () => ({
  useModelWhitelist: () => null,
  isModelWhitelisted: () => true,
}));
vi.mock("./WorkerModelPicker", async (load) => ({
  ...(await load<typeof import("./WorkerModelPicker")>()),
  useModelCatalogReader: () => ({}),
  useWorkerModels: () => ({
    models: ["gpt-6-astra", "gpt-6-luna"].map((model) => ({
      model,
      serviceTiers: [],
      supportedReasoningEfforts: [{ reasoningEffort: "high" }],
    })),
    loading: false,
    error: "",
    retry: vi.fn(),
  }),
}));
import { ExecutionSettings } from "./ExecutionSettings";
import { ExecutionSummaryTrigger } from "./ExecutionSummaryTrigger";

function children(node: ReactNode): ReactElement<any>[] {
  if (Array.isArray(node)) return node.flatMap(children);
  if (!isValidElement<{ children?: ReactNode }>(node)) return [];
  return [node, ...children(node.props.children)];
}
function invoke(element: ReactElement<any>) {
  const component = element.type as (props: any) => ReactElement<any>;
  h.scope = component.name + (element.key || "");
  h.cursor = 0;
  if (!h.cells.has(h.scope)) h.cells.set(h.scope, []);
  return component(element.props);
}
const initial: Agent = {
  id: "lead",
  isLead: true,
  source: "managed",
  accountKey: "default",
  model: "gpt-6-astra",
  effort: "medium",
  status: "idle",
};
function render(agent = initial) {
  const controller = invoke(
    createElement(ExecutionSettings, {
      agent,
      catalog: {
        models: ["gpt-6-astra", "gpt-6-luna"].map((model) => ({
          model,
          serviceTiers: [],
          supportedReasoningEfforts: [],
        })),
        loading: false,
        error: "",
        retry: vi.fn(),
      },
      refresh: h.refresh,
      renderTrigger: (trigger) =>
        createElement(ExecutionSummaryTrigger, {
          agent: trigger.agent,
          team: [],
          onOpen: trigger.onClick,
        }),
    }),
  );
  const menu = invoke(controller);
  const scoped = children(menu.props.children).find(
    (node) =>
      typeof node.type === "function" &&
      node.type.name === "ScopedExecutionSettings",
  )!;
  const scopedKey = "ScopedExecutionSettings" + (scoped.key || "");
  if (h.scoped && h.scoped !== scopedKey) {
    for (const cell of h.cells.get(h.scoped) || []) cell.cleanup?.();
    h.cells.delete(h.scoped);
  }
  h.scoped = scopedKey;
  const controls = invoke(scoped);
  const picker = children(controls).find(
    (node) =>
      typeof node.type === "function" && node.type.name === "AgentSetupPicker",
  )!;
  const trigger = invoke(menu.props.trigger);
  const model = children(trigger).find(
    (node) => node.props.className === "execution-model",
  )!.props.children;
  for (const effect of h.effects.splice(0)) effect();
  return {
    model,
    label: trigger.props["aria-label"],
    picker: picker.props,
    triggerAgent: menu.props.trigger.props.agent,
  };
}
afterEach(() => {
  for (const cells of h.cells.values())
    for (const cell of cells) cell.cleanup?.();
  h.cells.clear();
  h.scoped = "";
  h.effects = [];
  h.post.mockReset();
  h.refresh.mockClear();
});

test("the composer trigger uses the confirmed model before SSE and yields to remote settings", async () => {
  h.post.mockResolvedValue({ ...initial, model: "gpt-6-luna" });
  expect(render().model).toBe("Astra 6");
  render().picker.onModel("gpt-6-luna");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  expect(render().model).toBe("Luna 6");
  expect(render().label).toContain("Lead Luna 6");
  await new Promise<void>(queueMicrotask);
  expect(h.refresh).not.toHaveBeenCalled();
  expect(render({ ...initial, status: "running" }).model).toBe("Luna 6");
  expect(render({ ...initial, model: "gpt-6-luna" }).model).toBe("Luna 6");
  expect(render({ ...initial, model: "remote-model" }).model).toBe(
    "remote-model",
  );
});

test("a failed model save leaves the composer trigger on its canonical model", async () => {
  h.post.mockRejectedValue(new Error("Save failed"));
  render().picker.onModel("gpt-6-luna");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  expect(render().model).toBe("Astra 6");
});

test("queued settings and account changes keep their canonical scope", async () => {
  const active: Agent = {
    ...initial,
    status: "running",
    nextTurnSettingsSupported: true,
  };
  const queued: Agent = {
    ...active,
    pendingSettings: { model: "gpt-6-luna", effort: "high" },
    pendingSettingsAccountKey: "default",
  };
  h.post.mockResolvedValue(queued);
  render(active).picker.onModel("gpt-6-luna");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  expect(required(required(h.post.mock.calls[0])[1])).toMatchObject({
    next_turn: true,
  });
  expect(render(active).model).toBe("Luna 6");
  expect(render(queued).label).toContain("Lead Luna 6 High");
  expect(render({ ...initial, accountKey: "other" }).model).toBe("Astra 6");
  expect(render(initial).model).toBe("Astra 6");
});

test("switching picker roles keeps acknowledged worker defaults in the summary", async () => {
  h.post.mockResolvedValue({
    ...initial,
    workerDefaults: { model: "gpt-6-astra", effort: "high" },
  });
  render().picker.onRole("worker");
  render().picker.onModel("gpt-6-astra");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  expect(render().label).toContain("Workers Astra 6 High");
  render().picker.onRole("orchestrator");
  expect(render().label).toContain("Workers Astra 6 High");
  expect(render({ ...initial, id: "other-chat" }).label).toContain(
    "Workers Luna 6 High",
  );
});

test("review settings use their canonical response without a refresh", async () => {
  h.post.mockResolvedValue({
    ...initial,
    reviewDefaults: { model: "gpt-6-luna", effort: null },
  });
  render().picker.onRole("review");
  render().picker.onModel("gpt-6-luna");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  expect(render().label).toContain("Review Luna 6 Medium");
  await new Promise<void>(queueMicrotask);
  expect(h.refresh).not.toHaveBeenCalled();
});

test("unrelated earlier SSE does not suppress a later acknowledged model", async () => {
  let finish!: (agent: Agent) => void;
  h.post.mockImplementation(
    () =>
      new Promise<Agent>((resolve) => {
        finish = resolve;
      }),
  );
  render().picker.onModel("gpt-6-luna");
  const intermediate: Agent = {
    ...initial,
    workerDefaults: { model: "gpt-6-astra", effort: "high", fastMode: false },
  };
  expect(render(intermediate).model).toBe("Astra 6");
  finish({ ...intermediate, model: "gpt-6-luna" });
  await Promise.resolve();
  expect(render(intermediate).model).toBe("Luna 6");
  expect(render(intermediate).label).toContain("Workers Astra 6 High");
});

test("switching role before acknowledgment preserves the stable trigger receipt", async () => {
  let finish!: (agent: Agent) => void;
  h.post.mockImplementation(
    () =>
      new Promise<Agent>((resolve) => {
        finish = resolve;
      }),
  );
  const view = render();
  view.picker.onModel("gpt-6-luna");
  view.picker.onRole("worker");
  render();
  finish({ ...initial, model: "gpt-6-luna" });
  await Promise.resolve();
  expect(render().model).toBe("Luna 6");
});

test("newer same-field SSE wins over an older in-flight model response", async () => {
  let finish!: (agent: Agent) => void;
  h.post.mockImplementation(
    () =>
      new Promise<Agent>((resolve) => {
        finish = resolve;
      }),
  );
  render().picker.onModel("gpt-6-luna");
  const remote = { ...initial, model: "remote-model" };
  render(remote);
  finish({ ...initial, model: "gpt-6-luna" });
  await Promise.resolve();
  const current = render(remote);
  expect(current.model).toBe("remote-model");
  expect(current.picker.model).toBe("remote-model");
});

for (const changed of [
  { ...initial, id: "other-chat" },
  { ...initial, accountKey: "other" },
]) {
  test(`an old response cannot cross ${changed.id === initial.id ? "account" : "chat"} scope`, async () => {
    let finish!: (agent: Agent) => void;
    h.post.mockImplementation(
      () =>
        new Promise<Agent>((resolve) => {
          finish = resolve;
        }),
    );
    render().picker.onModel("gpt-6-luna");
    render(changed);
    finish({ ...initial, model: "gpt-6-luna" });
    await Promise.resolve();
    expect(render(changed).model).toBe("Astra 6");
    expect(render(initial).model).toBe("Astra 6");
  });
}

test("unchanged settings retain the trigger agent identity", async () => {
  expect(render().triggerAgent).toBe(initial);
  h.post.mockResolvedValue({ ...initial, model: "gpt-6-luna" });
  render().picker.onModel("gpt-6-luna");
  await Promise.resolve();
  const first = render().triggerAgent;
  expect(render().triggerAgent).toBe(first);
  expect(render().triggerAgent).toBe(first);
});

test("an older same-domain request cannot replace the latest acknowledgment", async () => {
  const finish: Array<(agent: Agent) => void> = [];
  h.post.mockImplementation(
    () =>
      new Promise<Agent>((resolve) => {
        finish.push(resolve);
      }),
  );
  render().picker.onModel("gpt-6-luna");
  render().picker.onRole("worker");
  render().picker.onRole("orchestrator");
  render().picker.onModel("gpt-6-astra");
  expect(finish).toHaveLength(2);
  required(finish[1])({ ...initial, model: "gpt-6-astra" });
  await Promise.resolve();
  required(finish[0])({ ...initial, model: "gpt-6-luna" });
  await Promise.resolve();
  expect(render().model).toBe("Astra 6");
});
