import { NativeSelect, Switch } from "@mantine/core";
import type { PostBody } from "../../apiTypes";
import type { ComponentHookCell } from "../componentTestFixtures";
import type { EffectCallback, PropsWithChildren, ComponentProps } from "react";
import assert from "node:assert/strict";
import { required } from "../componentTestFixtures";
import {
  createElement,
  isValidElement,
  type ReactElement,
  type ReactNode,
} from "react";
import { afterEach, expect, test, vi } from "vitest";
import type { Agent } from "../../types";

type SwitchFixtureProps = Omit<ComponentProps<typeof Switch>, "onChange"> & {
  onChange(event: {
    currentTarget: Pick<
      Parameters<
        NonNullable<ComponentProps<typeof Switch>["onChange"]>
      >[0]["currentTarget"],
      "checked"
    >;
  }): void;
};
type SelectFixtureProps = Omit<
  ComponentProps<typeof NativeSelect>,
  "onChange"
> & {
  onChange(event: {
    currentTarget: Pick<
      Parameters<
        NonNullable<ComponentProps<typeof NativeSelect>["onChange"]>
      >[0]["currentTarget"],
      "value"
    >;
  }): void;
};
const h = vi.hoisted(() => ({
  cells: new Map<string, ComponentHookCell[]>(),
  storage: new Map<string, unknown>(),
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
    if (!previous || deps.some((value, i) => value !== previous.deps?.[i]))
      cells[index] = { deps, value: compute() };
    return required(cells[index]).value;
  },
  useEffect: (effect: EffectCallback, deps: unknown[]) => {
    const cells = h.cells.get(h.scope)!;
    const index = h.cursor++;
    const previous = cells[index];
    if (
      !previous ||
      !deps ||
      deps.some((value, i) => value !== previous.deps?.[i])
    ) {
      previous?.cleanup?.();
      const cell: ComponentHookCell = { deps };
      cells[index] = cell;
      h.effects.push(() => {
        cell.cleanup = effect() || undefined;
      });
    }
  },
  useLayoutEffect: () => {},
  useId: () => "settings-role",
  useCallback: (callback: unknown) => callback,
}));
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerStorage: () => ({
    setItem: (key: string, raw: string) => h.storage.set(key, JSON.parse(raw)),
    removeItem: (key: string) => h.storage.delete(key),
  }),
  useServerApi: () => ({
    post: h.post,
    saved: (key: string, fallback: unknown) => h.storage.get(key) ?? fallback,
    save: (key: string, value: unknown) => h.storage.set(key, value),
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
      serviceTiers: [{ id: "priority" }],
      supportedReasoningEfforts: [{ reasoningEffort: "high" }],
    })),
    loading: false,
    error: "",
    retry: vi.fn(),
  }),
}));
import { AgentSetupPicker } from "./AgentSetupPicker";
import { ExecutionSettings, ExecutionPermissions } from "./ExecutionSettings";
import { ClaudeSettings } from "../ClaudeSettings";
import { ExecutionSummaryTrigger } from "./ExecutionSummaryTrigger";

function children(
  node: ReactNode,
): ReactElement<PropsWithChildren<Record<string, unknown>>>[] {
  if (Array.isArray(node)) return node.flatMap(children);
  if (!isValidElement<{ children?: ReactNode }>(node)) return [];
  return [node, ...children(node.props.children)];
}
function invoke(element: ReactElement<unknown>) {
  const component: unknown = element.type;
  assert.ok(typeof component === "function");
  h.scope = component.name + (element.key || "");
  h.cursor = 0;
  if (!h.cells.has(h.scope)) h.cells.set(h.scope, []);
  const result: unknown = component(element.props);
  assert.ok(isValidElement<PropsWithChildren<Record<string, unknown>>>(result));
  return result;
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
function render(
  agent?: Agent,
  permissions?: false,
  onDraftChange?: ComponentProps<typeof ExecutionSettings>["onDraftChange"],
): {
  controls: ReturnType<typeof invoke>;
  model: ReactNode;
  label: unknown;
  picker: ComponentProps<typeof AgentSetupPicker>;
  triggerAgent: Agent;
};
function render(
  agent: Agent,
  permissions: true,
): {
  controls: ReturnType<typeof invoke>;
  model: string;
  label: string;
  picker: Partial<ComponentProps<typeof AgentSetupPicker>>;
  triggerAgent: Agent;
};
function render(
  agent = initial,
  permissions = false,
  onDraftChange?: ComponentProps<typeof ExecutionSettings>["onDraftChange"],
) {
  const controller = invoke(
    createElement(permissions ? ExecutionPermissions : ExecutionSettings, {
      agent,
      catalog: {
        models: ["gpt-6-astra", "gpt-6-luna"].map((model) => ({
          model,
          serviceTiers: [{ id: "priority" }],
          supportedReasoningEfforts: [
            { reasoningEffort: "medium" },
            { reasoningEffort: "high" },
          ],
        })),
        loading: false,
        error: "",
        retry: vi.fn(),
      },
      refresh: h.refresh,
      onDraftChange,
      renderTrigger: (trigger) =>
        createElement(ExecutionSummaryTrigger, {
          agent: trigger.agent,
          team: [],
          onOpen: trigger.onClick,
        }),
    }),
  );
  const menu = invoke(controller);
  const scoped = (permissions ? [menu] : children(menu.props.children)).find(
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
  if (permissions)
    return {
      controls,
      model: "",
      label: "",
      picker: {},
      triggerAgent: agent,
    };
  const picker = children(controls).find(
    (node) =>
      typeof node.type === "function" && node.type.name === "AgentSetupPicker",
  )!;
  assert.ok(
    isValidElement<ComponentProps<typeof ExecutionSummaryTrigger>>(
      menu.props.trigger,
    ),
  );
  assert.ok(
    isValidElement<ComponentProps<typeof AgentSetupPicker>>(picker) &&
      picker.type === AgentSetupPicker,
  );
  const trigger = invoke(menu.props.trigger);
  const model = children(trigger).find(
    (node) => node.props.className === "execution-model",
  )!.props.children;
  for (const effect of h.effects.splice(0)) effect();
  return {
    controls,
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
  h.storage.clear();
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

for (const draft of [false, true]) {
  test(`inherited review reasoning stays hidden while Fast remains editable in ${draft ? "new chat drafts" : "saved settings"}`, async () => {
    const agent: Agent = {
      ...initial,
      fastMode: true,
      reviewDefaults: { model: null, effort: null },
    };
    const onDraftChange = draft ? vi.fn() : undefined;
    h.post.mockResolvedValue({
      ...agent,
      reviewDefaults: { model: null, effort: null, fastMode: false },
    });
    render(agent, false, onDraftChange).picker.onRole("review");
    const picker = render(agent, false, onDraftChange).picker;
    expect(picker.efforts).toEqual([]);
    expect(picker.fastAvailable).toBe(true);
    picker.onFast();
    if (onDraftChange) {
      expect(onDraftChange).toHaveBeenCalledWith({
        review_defaults: { model: null, effort: null, fast_mode: false },
      });
      expect(h.post).not.toHaveBeenCalled();
    } else {
      await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
      expect(required(required(h.post.mock.calls[0])[1])).toMatchObject({
        review_defaults: { model: null, effort: null, fast_mode: false },
      });
    }
  });
}

for (const role of ["orchestrator", "worker", "review"] as const) {
  test(`${role} saves explicit Fast false and retains the canonical response before SSE`, async () => {
    const agent: Agent = {
      ...initial,
      fastMode: true,
      workerDefaults: { model: "gpt-6-astra", effort: "high", fastMode: true },
      reviewDefaults: { model: "gpt-6-astra", effort: "high", fastMode: true },
    };
    const canonical: Agent = {
      ...agent,
      ...(role === "orchestrator"
        ? { fastMode: false }
        : role === "worker"
          ? {
              workerDefaults: {
                model: "gpt-6-astra",
                effort: "high",
                fastMode: false,
              },
            }
          : {
              reviewDefaults: {
                model: "gpt-6-astra",
                effort: "high",
                fastMode: false,
              },
            }),
    };
    h.post.mockResolvedValue(canonical);
    render(agent).picker.onRole(role);
    expect(render(agent).picker.fastAvailable).toBe(true);
    expect(render(agent).picker.fast).toBe(true);
    expect(render(agent).label).toContain(
      role === "orchestrator"
        ? "Lead Astra 6 Medium (codex) Fast"
        : role === "worker"
          ? "Workers Astra 6 High (codex) Fast"
          : "Review Astra 6 High (codex) Fast",
    );
    render(agent).picker.onFast();
    await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
    const body: PostBody<"/api/conversation"> = required(
      required(h.post.mock.calls[0])[1],
    );
    expect(
      role === "review"
        ? body.review_defaults?.fast_mode
        : role === "worker"
          ? body.worker_defaults?.fast_mode
          : body.fast_mode,
    ).toBe(false);
    expect(render(agent).picker.fast).toBe(false);
    expect(render(agent).label).not.toContain(
      role === "orchestrator"
        ? "Lead Astra 6 Medium (codex) Fast"
        : role === "worker"
          ? "Workers Astra 6 High (codex) Fast"
          : "Review Astra 6 High (codex) Fast",
    );
    expect(h.refresh).not.toHaveBeenCalled();
    expect(render(canonical).picker.fast).toBe(false);
  });
}

test("review Fast receipts yield to a newer Fast-only SSE update", async () => {
  const agent: Agent = {
    ...initial,
    reviewDefaults: { model: "gpt-6-astra", effort: null, fastMode: false },
  };
  h.post.mockResolvedValue({
    ...agent,
    reviewDefaults: { ...agent.reviewDefaults, fastMode: true },
  });
  render(agent).picker.onRole("review");
  render(agent).picker.onFast();
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  expect(render(agent).picker.fast).toBe(true);
  expect(
    render({
      ...agent,
      reviewDefaults: { model: "gpt-6-astra", effort: null, fastMode: true },
    }).picker.fast,
  ).toBe(true);
  expect(render(agent).picker.fast).toBe(false);
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
] as const) {
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

for (const status of ["running", "approval"] as const) {
  test(`Codex permissions remain enabled while ${status}`, () => {
    const controls = render(
      {
        ...initial,
        status,
        inFlight: true,
        yoloMode: false,
      },
      true,
    ).controls;
    const permission = children(controls).find(
      (node): node is ReactElement<SwitchFixtureProps> =>
        node.props["aria-label"] === "Full access without approval",
    )!;
    expect(permission).toBeDefined();
    expect(permission.props.disabled).toBe(false);
  });
}

for (const status of ["running", "approval"] as const) {
  test(`Claude permissions remain enabled while ${status}`, async () => {
    h.post.mockResolvedValue({
      settings: { permissionMode: "default" },
      turns: [],
      tasks: [],
    });
    const element = createElement(ClaudeSettings, {
      agent: { ...initial, provider: "claude", status, inFlight: true },
    });
    invoke(element);
    for (const effect of h.effects.splice(0)) effect();
    await new Promise<void>(queueMicrotask);
    const controls = invoke(element);
    const permission = children(controls).find(
      (node): node is ReactElement<SelectFixtureProps> =>
        node.props.label === "Permission mode",
    )!;
    expect(permission).toBeDefined();
    expect(permission.props.disabled).toBe(false);
  });
}

test("Codex explains when Ask takes effect for a turn started with Full", () => {
  const controls = render(
    {
      ...initial,
      status: "running",
      inFlight: true,
      yoloMode: false,
    },
    true,
  ).controls;
  const permission = children(controls).find(
    (node): node is ReactElement<SwitchFixtureProps> =>
      node.props["aria-label"] === "Full access without approval",
  )!;
  expect(permission.props.description).toBe(
    "Ask policy changes apply from the next turn.",
  );
});

test("Codex describes the selected Ask mode truthfully", () => {
  const controls = render({ ...initial, yoloMode: false }, true).controls;
  const permission = children(controls).find(
    (node): node is ReactElement<SwitchFixtureProps> =>
      node.props["aria-label"] === "Full access without approval",
  )!;
  expect(permission.props.description).toBe("Tools ask for permission.");
});

test("a superseding Ask intent prevents reuse of a lost Full request identity", async () => {
  const control = (agent: Agent) =>
    children(render(agent, true).controls).find(
      (node): node is ReactElement<SwitchFixtureProps> =>
        node.props["aria-label"] === "Full access without approval",
    )!;
  const ask = { ...initial, yoloMode: false };
  const full = { ...initial, yoloMode: true };
  h.post.mockRejectedValueOnce(new Error("response lost"));
  control(ask).props.onChange({ currentTarget: { checked: true } });
  await vi.waitFor(() => expect(control(full).props.disabled).toBe(false));
  const lostFullId = h.post.mock.calls[0][1].request_id;
  h.post.mockResolvedValueOnce(ask);
  control(full).props.onChange({ currentTarget: { checked: false } });
  await vi.waitFor(() => expect(control(ask).props.disabled).toBe(false));
  h.post.mockResolvedValueOnce(full);
  control(ask).props.onChange({ currentTarget: { checked: true } });
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledTimes(3));
  expect(h.post.mock.calls[2][1].request_id).not.toBe(lostFullId);
});

for (const [first, opposite] of [
  ["bypassPermissions", "default"],
  ["default", "bypassPermissions"],
] as const) {
  test(`Claude replaces a superseded lost ${first} intent after reopening`, async () => {
    let persisted: Extract<
      PostBody<"/api/claude/session">,
      { action: "settings" }
    >["settings"]["permissionMode"] = opposite;
    let loseResponse = true;
    const mutations: Extract<
      PostBody<"/api/claude/session">,
      { action: "settings" }
    >[] = [];
    h.post.mockImplementation(
      async (_path: string, body: PostBody<"/api/claude/session">) => {
        if (body.action === "settings") {
          mutations.push(body);
          persisted = body.settings.permissionMode;
          if (loseResponse) throw new Error("response lost");
          return {};
        }
        return {
          settings: { permissionMode: persisted },
          turns: [],
          tasks: [],
        };
      },
    );
    const element = createElement(ClaudeSettings, {
      agent: { ...initial, provider: "claude" },
    });
    const control = () =>
      children(invoke(element)).find(
        (node): node is ReactElement<SelectFixtureProps> =>
          node.props.label === "Permission mode",
      )!;
    const reopen = async () => {
      for (const cell of h.cells.get("ClaudeSettings") || []) cell.cleanup?.();
      h.cells.delete("ClaudeSettings");
      invoke(element);
      for (const effect of h.effects.splice(0)) effect();
      await vi.waitFor(() => expect(control().props.disabled).toBe(false));
    };
    const choose = async (mode: string) => {
      control().props.onChange({ currentTarget: { value: mode } });
      await vi.waitFor(() => expect(control().props.disabled).toBe(false));
    };
    await reopen();
    await choose(first);
    await choose(first); // The same unresolved click retains its request identity.
    expect(mutations[1].request_id).toBe(mutations[0].request_id);
    await reopen();
    loseResponse = false;
    await choose(opposite);
    await choose(first);
    expect(mutations[2].request_id).not.toBe(mutations[0].request_id);
    expect(mutations[3].request_id).not.toBe(mutations[0].request_id);
    expect(mutations[3].request_id).not.toBe(mutations[2].request_id);
  });
}

test("a late Claude success preserves the newer unresolved permission request", async () => {
  let persisted: Extract<
    PostBody<"/api/claude/session">,
    { action: "settings" }
  >["settings"]["permissionMode"] = "default";
  let finish!: (value: unknown) => void;
  const mutations: Extract<
    PostBody<"/api/claude/session">,
    { action: "settings" }
  >[] = [];
  h.post.mockImplementation(
    (_path: string, body: PostBody<"/api/claude/session">) => {
      if (body.action === "settings") {
        mutations.push(body);
        persisted = body.settings.permissionMode;
        return mutations.length === 1
          ? new Promise((resolve) => {
              finish = resolve;
            })
          : Promise.reject(new Error("response lost"));
      }
      return Promise.resolve({
        settings: { permissionMode: persisted },
        turns: [],
        tasks: [],
      });
    },
  );
  const element = createElement(ClaudeSettings, {
    agent: { ...initial, provider: "claude" },
  });
  const control = () =>
    children(invoke(element)).find(
      (node): node is ReactElement<SelectFixtureProps> =>
        node.props.label === "Permission mode",
    )!;
  const reopen = async () => {
    for (const cell of h.cells.get("ClaudeSettings") || []) cell.cleanup?.();
    h.cells.delete("ClaudeSettings");
    invoke(element);
    for (const effect of h.effects.splice(0)) effect();
    await vi.waitFor(() => expect(control().props.disabled).toBe(false));
  };
  await reopen();
  control().props.onChange({ currentTarget: { value: "bypassPermissions" } });
  await reopen();
  control().props.onChange({ currentTarget: { value: "default" } });
  await vi.waitFor(() => expect(control().props.disabled).toBe(false));
  const unresolvedId = mutations[1].request_id;
  expect(unresolvedId).not.toBe(mutations[0].request_id);
  finish({});
  await Promise.resolve();
  control().props.onChange({ currentTarget: { value: "default" } });
  await vi.waitFor(() => expect(mutations).toHaveLength(3));
  expect(mutations[2].request_id).toBe(unresolvedId);
});
