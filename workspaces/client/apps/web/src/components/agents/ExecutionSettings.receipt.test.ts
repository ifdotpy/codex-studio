import {
  AgentSource,
  AgentStatus,
  ProviderKind,
  TransferScope,
  TransferStatus,
} from "../../generated/api";
import { Button, NativeSelect, Switch, Textarea } from "@mantine/core";
import type { PostBody, PostResult } from "../../apiTypes";
import { ApiError } from "../../api";
import * as appLog from "../../appLog";
import { AccountTransferStatus } from "../Accounts";
import { required, type ComponentHookCell } from "../componentTestFixtures";
import type { EffectCallback, PropsWithChildren, ComponentProps } from "react";
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
  report: vi.fn(),
  read: vi.fn(),
  post: vi.fn(),
  refresh: vi.fn(async () => {}),
  emptyCatalog: false,
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
    if (!previous || deps.some((value, i) => value !== previous.deps![i]))
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
      const cell: ComponentHookCell = { deps, cleanup: undefined };
      cells[index] = cell;
      h.effects.push(() => {
        cell.cleanup = effect() as ComponentHookCell["cleanup"];
      });
    }
  },
  useLayoutEffect: () => {},
  useId: () => "settings-role",
  useCallback: (callback: unknown) => callback,
}));
vi.mock("../../appLog", () => ({ appendAppLog: h.report }));
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
vi.mock("@mantine/hooks", async (load) => ({
  ...(await load<typeof import("@mantine/hooks")>()),
  useMediaQuery: () => false,
}));
vi.mock("../models/modelChoices", () => ({
  useModelChoices: () => ({}),
  isModelEnabled: () => true,
}));
vi.mock("./WorkerModelPicker", async (load) => ({
  ...(await load<typeof import("./WorkerModelPicker")>()),
  useModelCatalogReader: () => ({ scope: "settings-receipt", read: h.read }),
  useWorkerModels: () => ({
    models: (h.emptyCatalog ? [] : ["gpt-6-astra", "gpt-6-luna"]).map(
      (model) => ({
        model,
        serviceTiers: [{ id: "priority" }],
        supportedReasoningEfforts: [{ reasoningEffort: "high" }],
      }),
    ),
    loading: false,
    error: "",
    retry: vi.fn(),
  }),
}));
import { AgentSetupPicker } from "./AgentSetupPicker";
import { ExecutionSettings, ExecutionPermissions } from "./ExecutionSettings";
import { ClaudeSettings } from "../ClaudeSettings";
import { ExecutionSummaryTrigger } from "./ExecutionSummaryTrigger";
import { ChatSettingsDialog } from "./ChatSettingsDialog";
import {
  WorkerLaunchDialog,
  type WorkerLaunch,
} from "../shell/WorkerLaunchDialog";
import { componentSnapshot } from "../componentTestFixtures";
import { WorkerPermissions } from "../shell/WorkerPermissions";

function children(
  node: ReactNode,
): ReactElement<PropsWithChildren<Record<string, unknown>>>[] {
  if (Array.isArray(node)) return node.flatMap(children);
  if (!isValidElement<{ children?: ReactNode }>(node)) return [];
  return [node, ...children(node.props.children)];
}
function invoke(element: ReactElement<unknown>) {
  const component = element.type as (
    props: typeof element.props,
  ) => ReactElement<PropsWithChildren<Record<string, unknown>>>;
  h.scope = component.name + (element.key || "");
  h.cursor = 0;
  if (!h.cells.has(h.scope)) h.cells.set(h.scope, []);
  return component(element.props);
}
const initial: Agent = {
  id: "lead",
  isLead: true,
  provider: ProviderKind.codex,
  source: AgentSource.managed,
  accountKey: "default",
  model: "gpt-6-astra",
  effort: "medium",
  status: AgentStatus.idle,
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
  statusModal: ReactNode;
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
  statusModal: ReactNode;
};
function render(
  agent = initial,
  permissions = false,
  onDraftChange?: ComponentProps<typeof ExecutionSettings>["onDraftChange"],
): {
  controls: ReturnType<typeof invoke>;
  model: ReactNode;
  label: unknown;
  picker: Partial<ComponentProps<typeof AgentSetupPicker>>;
  triggerAgent: Agent;
  statusModal: ReactNode;
} {
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
      ...(onDraftChange && { onDraftChange }),
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
      statusModal: null,
      triggerAgent: agent,
    };
  const picker = children(controls).find(
    (node) =>
      typeof node.type === "function" && node.type.name === "AgentSetupPicker",
  )!;

  const trigger = invoke(
    menu.props.trigger as ReactElement<
      ComponentProps<typeof ExecutionSummaryTrigger>
    >,
  );
  const model = children(trigger).find(
    (node) => node.props.className === "execution-model",
  )!.props.children;
  for (const effect of h.effects.splice(0)) effect();
  return {
    controls,
    model,
    label: trigger.props["aria-label"],
    picker: picker.props as ComponentProps<typeof AgentSetupPicker>,
    statusModal: isValidElement(menu.props.statusModal)
      ? menu.props.statusModal
      : null,
    triggerAgent: (
      menu.props.trigger as ReactElement<
        ComponentProps<typeof ExecutionSummaryTrigger>
      >
    ).props.agent,
  };
}
afterEach(() => {
  for (const cells of h.cells.values())
    for (const cell of cells) cell.cleanup?.();
  h.cells.clear();
  h.storage.clear();
  h.scoped = "";
  h.effects = [];
  h.report.mockClear();
  h.read.mockReset();
  h.post.mockReset();
  h.refresh.mockClear();
  h.emptyCatalog = false;
});

test("transfer actions retain response identity and retry the same request after a lost response", async () => {
  const operation = {
    id: "saved-transfer",
    leadId: initial.id,
    targetAccountKey: "work",
    scope: TransferScope.team,
    status: TransferStatus.pending,
    created: 1,
    updated: 2,
    members: {},
    canRetry: true,
  } satisfies PostResult<"/api/agents/account-transfer">;
  const agent: Agent = { ...initial, accountTransfer: operation };
  const transfer = () => {
    const status = children(render(agent).controls).find(
      (
        node,
      ): node is ReactElement<ComponentProps<typeof AccountTransferStatus>> =>
        node.type === AccountTransferStatus,
    );
    return required(status).props;
  };
  expect(transfer()).toHaveProperty("opened", false);
  h.post.mockRejectedValueOnce(new Error("response lost"));
  transfer().onAction?.("retry");
  await vi.waitFor(() => expect(transfer().pending).toBe(false));
  expect(transfer().transfer).toBe(operation);
  expect(h.report).toHaveBeenCalledWith(
    expect.objectContaining({ level: "error", reason: "response lost" }),
  );
  const response = {
    ...operation,
    updated: 3,
    waiting: "Waiting for the current turn",
  };
  h.post.mockResolvedValueOnce(response);
  transfer().onAction?.("retry");
  await vi.waitFor(() => expect(transfer().pending).toBe(false));
  expect(h.post).toHaveBeenCalledTimes(2);
  expect(h.post.mock.calls[0]).toEqual(h.post.mock.calls[1]);
  expect(h.post).toHaveBeenLastCalledWith("/api/agents/account-transfer", {
    action: "retry",
    scope: "team",
    request_id: operation.id,
  });
  expect(transfer().transfer).toBe(response);
});

test("a remote transfer does not open the modal", () => {
  render(initial);
  const agent = {
    ...initial,
    accountTransfer: {
      id: "remote",
      status: TransferStatus.pending,
      scope: TransferScope.team,
    },
  };
  render(agent);
  const controls = render(agent).controls;
  const modal = children(controls).find(
    (node) => node.type === AccountTransferStatus,
  );
  expect(required(modal).props.opened).toBe(false);
});

test("the transfer modal shows an action failure even without a start retry", () => {
  const modal = AccountTransferStatus({
    transfer: { id: "transfer", status: "pending" },
    error: "Cancel response lost",
    onAction: () => {},
  });
  expect(
    children(modal).some(
      (node) =>
        node.props.role === "alert" &&
        node.props.children === "Cancel response lost",
    ),
  ).toBe(true);
});

test("blocked members are reported but provider mismatches are not", () => {
  render({
    ...initial,
    accountTransfer: {
      id: "transfer",
      status: "pending",
      scope: TransferScope.team,
      blocked: [{ id: "blocked", name: "Worker", reason: "Receipt missing" }],
      leftOnSource: [
        {
          id: "left",
          name: "Other",
          provider: ProviderKind.claude,
          reason: "Uses claude",
        },
      ],
    },
  });
  expect(h.report).toHaveBeenCalledWith(
    expect.objectContaining({
      level: "error",
      reason: "Worker: Receipt missing",
    }),
  );
  expect(h.report).not.toHaveBeenCalledWith(
    expect.objectContaining({ reason: expect.stringContaining("Uses claude") }),
  );
});

for (const provider of ["Future Provider", "codex", "claude"]) {
  test(`a cold execution summary preserves the explicit ${provider} identity`, () => {
    h.emptyCatalog = true;
    const agent: Agent = {
      ...initial,
      // @ts-expect-error Future identities may arrive at the display boundary.
      provider,
    };
    const trigger = invoke(
      createElement(ExecutionSummaryTrigger, {
        agent,
        team: [],
        onOpen: () => {},
      }),
    );
    expect(trigger.props["aria-label"]).toContain(
      `Lead Astra 6 Medium (${provider})`,
    );
    expect(trigger.props.title).toBe(trigger.props["aria-label"]);
  });
}

const unsupportedAgents: Agent[] = [
  { ...initial, id: "missing-provider", provider: undefined, yoloMode: false },
  { ...initial, id: "null-provider", provider: null, yoloMode: false },
  {
    ...initial,
    id: "unknown-provider",
    yoloMode: false,
    // @ts-expect-error A future wire identity must not gain Codex permission actions.
    provider: "future-provider",
  },
];

for (const agent of unsupportedAgents) {
  test(`${agent.id} cannot supply permissions for a known worker provider`, () => {
    for (const provider of ["codex", "claude"]) {
      expect(
        invoke(
          createElement(WorkerPermissions, {
            lead: agent,
            provider,
            reviewer: false,
            value: undefined,
            onChange: () => {},
          }),
        ),
      ).toBeNull();
    }
  });
  test(`${agent.id} cannot enable Daybreak through the model picker or extras`, () => {
    const result = render({ ...agent, daybreakEnabled: true });
    expect(result.picker.daybreakAvailable).toBe(false);
    expect(
      children(result.controls).some(
        (node) => node.props["aria-label"] === "Daybreak",
      ),
    ).toBe(false);
    result.picker.onDaybreak?.();
    expect(h.post).not.toHaveBeenCalled();
  });
}

test("Daybreak remains available for a Codex main agent and absent for Claude", () => {
  expect(
    render({ ...initial, daybreakEnabled: true }).picker.daybreakAvailable,
  ).toBe(true);
  expect(
    render({ ...initial, provider: "claude", daybreakEnabled: true }).picker
      .daybreakAvailable,
  ).toBe(false);
});

function chatSettings(agent: Agent) {
  return invoke(
    createElement(ChatSettingsDialog, {
      settingsOpen: true,
      accountModalOpen: false,
      setSettingsOpen: () => {},
      agent,
      accounts: {
        data: {
          accounts: [],
          archivedAccounts: [],
          defaultAccountKey: "default",
          defaultAccountGroups: [],
          logins: [],
          supportsDelete: true,
          supportsDisconnect: true,
        },
        setData: () => {},
        error: "",
        refresh: async () => null,
        scope: "fixture",
      },
      notify: () => {},
      agentModels: { models: [], loading: false, error: "", retry: () => {} },
      data: componentSnapshot([agent]),
      refresh: h.refresh,
      workspaceId: "workspace",
      creating: false,
      newChat: () => {},
      project: () => {},
      setClaudeLoginKey: () => {},
    }),
  );
}

for (const agent of unsupportedAgents) {
  test(`${agent.id} cannot receive a Codex permission control through chat settings`, () => {
    expect(
      children(chatSettings(agent)).filter(
        (node) => node.type === ExecutionPermissions,
      ),
    ).toEqual([]);
  });
  test(`${agent.id} cannot post Full access from the actual permissions consumer`, async () => {
    h.post.mockResolvedValue({ ...agent, yoloMode: true });
    const permission = children(render(agent, true).controls).find(
      (node): node is ReactElement<SwitchFixtureProps> =>
        node.props["aria-label"] === "Full access without approval",
    );
    permission?.props.onChange({ currentTarget: { checked: true } });
    await Promise.resolve();
    expect(h.post).not.toHaveBeenCalled();
    expect(permission).toBeUndefined();
  });
}

test("chat settings retains the known Codex permission caller and the Claude settings branch", () => {
  expect(
    children(chatSettings({ ...initial, yoloMode: false })).some(
      (node) => node.type === ExecutionPermissions,
    ),
  ).toBe(true);
  const claude = children(
    chatSettings({ ...initial, provider: ProviderKind.claude }),
  );
  expect(claude.some((node) => node.type === ExecutionPermissions)).toBe(false);
  expect(
    claude.some(
      (node) => typeof node.type === "object" && node.props.agent !== undefined,
    ),
  ).toBe(true);
});

type TaskInputProps = Omit<ComponentProps<typeof Textarea>, "onChange"> & {
  onChange(event: {
    target: Pick<
      Parameters<
        NonNullable<ComponentProps<typeof Textarea>["onChange"]>
      >[0]["target"],
      "value"
    >;
  }): void;
};
type LaunchFormProps = {
  onSubmit(event: { preventDefault(): void }): Promise<void>;
};

async function submitWorker(launch: WorkerLaunch, lead: Agent) {
  const onStart = vi.fn(async (_body: PostBody<"/api/agents">) => {});
  const renderDialog = () =>
    invoke(
      createElement(WorkerLaunchDialog, {
        launch,
        lead,
        busy: false,
        onClose: () => {},
        onStart,
      }),
    );
  const input = children(renderDialog()).find(
    (node): node is ReactElement<TaskInputProps> => node.type === Textarea,
  );
  expect(input).toBeDefined();
  input?.props.onChange({ target: { value: "Read the project and report." } });
  const nodes = children(renderDialog());
  const button = nodes.find(
    (node) => node.type === Button && node.props.type === "submit",
  );
  const form = nodes.find(
    (node): node is ReactElement<LaunchFormProps> => node.type === "form",
  );
  expect(form).toBeDefined();
  await form?.props.onSubmit({ preventDefault: () => {} });
  return { onStart, button };
}

for (const provider of ["", "future-provider"]) {
  test(`an unresolved ${provider || "blank"} worker profile cannot submit its form`, async () => {
    const { onStart, button } = await submitWorker(
      {
        id: "worker",
        profile_id: "profile",
        name: "Worker",
        provider,
        reviewer: false,
      },
      initial,
    );
    expect(onStart).not.toHaveBeenCalled();
    expect(button?.props.disabled).toBe(true);
  });
}
for (const agent of unsupportedAgents) {
  test(`a Codex worker cannot submit with ${agent.id} lead identity`, async () => {
    const { onStart, button } = await submitWorker(
      {
        id: "worker",
        profile_id: "profile",
        name: "Worker",
        provider: "codex",
        reviewer: false,
      },
      agent,
    );
    expect(onStart).not.toHaveBeenCalled();
    expect(button?.props.disabled).toBe(true);
  });
}
for (const provider of ["codex", "claude"]) {
  test(`a supported ${provider} worker preserves its submitted parent and profile`, async () => {
    const { onStart, button } = await submitWorker(
      {
        id: "worker",
        profile_id: "profile",
        name: "Worker",
        provider,
        reviewer: false,
      },
      initial,
    );
    expect(button?.props.disabled).toBe(false);
    expect(onStart).toHaveBeenCalledWith(
      expect.objectContaining({
        id: "worker",
        profile_id: "profile",
        parent: initial.id,
        prompt: "Read the project and report.",
      }),
    );
  });
}

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
    status: AgentStatus.running,
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

for (const role of ["implementer", "reviewer"] as const) {
  test(`${role} queued settings update its own summary before SSE`, async () => {
    const agent: Agent = {
      ...initial,
      id: role,
      isLead: false,
      role,
      yoloMode: false,
      status: AgentStatus.running,
      nextTurnSettingsSupported: true,
    };
    const queued: Agent = {
      ...agent,
      pendingSettings: { model: "gpt-6-luna", effort: "high", fastMode: true },
      pendingSettingsAccountKey: "default",
    };
    h.post.mockResolvedValue(queued);
    render(agent).picker.onModel("gpt-6-luna");
    await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
    expect(required(h.post.mock.calls[0])[0]).toBe("/api/conversation");
    expect(required(required(h.post.mock.calls[0])[1])).toMatchObject({
      id: role,
      next_turn: true,
    });
    expect(required(required(h.post.mock.calls[0])[1])).not.toHaveProperty(
      "yolo_mode",
    );
    expect(render(agent).triggerAgent.yoloMode).toBe(false);
    expect(render(agent).model).toBe("Luna 6");
    expect(render(agent).label).toContain("Agent Luna 6 High");
  });
}

test("a direct settings save shows Saved instead of Queued", async () => {
  h.post.mockResolvedValue({ ...initial, model: "gpt-6-luna" });
  render(initial).picker.onModel("gpt-6-luna");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  await Promise.resolve();
  expect(
    children(render(initial).statusModal).map((node) => node.props.children),
  ).toContain("Saved");
});

test("late success cannot revive a failed account change after returning to the original account", async () => {
  let finish!: (agent: Agent) => void;
  h.post.mockImplementationOnce(
    () =>
      new Promise<Agent>((resolve) => {
        finish = resolve;
      }),
  );
  const agent: Agent = {
    ...initial,
    status: AgentStatus.running,
    nextTurnSettingsSupported: true,
  };
  render(agent).picker.onModel("gpt-6-luna");
  const other = { ...agent, accountKey: "other" };
  render(other);
  render(other);
  render(agent);
  finish({
    ...agent,
    pendingSettings: { model: "gpt-6-luna", effort: "medium", fastMode: false },
    pendingSettingsAccountKey: "default",
  });
  await Promise.resolve();
  expect(
    children(render(agent).statusModal).map((node) => node.props.children),
  ).toContain("Failed");
});

test("earlier matching pending and applied events cannot confirm an unresolved save", async () => {
  const agent: Agent = {
    ...initial,
    fastMode: false,
    status: AgentStatus.running,
    nextTurnSettingsSupported: true,
  };
  h.post.mockImplementationOnce(() => new Promise<Agent>(() => {}));
  render(agent).picker.onFast();
  const oldPending: Agent = {
    ...agent,
    pendingSettings: {
      model: agent.model,
      effort: agent.effort,
      fastMode: true,
    },
    pendingSettingsAccountKey: "default",
  };
  render(oldPending);
  render({ ...agent, fastMode: true });
  const modal = render({ ...agent, fastMode: true }).statusModal;
  expect(children(modal).map((node) => node.props.children)).toContain(
    "Saving…",
  );
});

test("a recovered next-turn receipt confirms the modal after settings already applied", async () => {
  const agent: Agent = {
    ...initial,
    fastMode: true,
    status: AgentStatus.idle,
    nextTurnSettingsSupported: true,
  };
  const request = {
    id: agent.id,
    next_turn: true,
    request_id: "recover-1",
    model: agent.model,
    effort: agent.effort,
    fast_mode: true,
  };
  h.storage.set(
    `next-turn-settings:${JSON.stringify([agent.id, "default"])}`,
    request,
  );
  h.post.mockResolvedValue(agent);
  const retry = children(render(agent).controls).find(
    (node) =>
      node.type === Button && node.props.children === "Check settings save",
  );
  expect(retry).toBeDefined();
  (required(retry).props.onClick as () => void)();
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  await Promise.resolve();
  const modal = render(agent).statusModal;
  expect(children(modal).map((node) => node.props.children)).toContain(
    "Queued for the next turn",
  );
});

test("a live change opens a modal and follows agent updates", async () => {
  const agent: Agent = {
    ...initial,
    status: AgentStatus.running,
    nextTurnSettingsSupported: true,
  };
  h.post.mockResolvedValue({
    ...agent,
    pendingSettings: { model: "gpt-6-luna", effort: "medium", fastMode: false },
    pendingSettingsAccountKey: "default",
  });
  render(agent).picker.onModel("gpt-6-luna");
  let modal = render(agent).statusModal;
  expect(children(modal).map((node) => node.props.children)).toContain(
    "Saving…",
  );
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  const queued: Agent = {
    ...agent,
    pendingSettings: { model: "gpt-6-luna", effort: "medium", fastMode: false },
    pendingSettingsAccountKey: "default",
  };
  render(queued);
  modal = render(queued).statusModal;
  expect(children(modal).map((node) => node.props.children)).toContain(
    "Queued for the next turn",
  );
  const applied: Agent = {
    ...agent,
    model: "gpt-6-luna",
    effort: "medium",
    fastMode: false,
  };
  render(applied);
  modal = render(applied).statusModal;
  expect(children(modal).map((node) => node.props.children)).toContain(
    "Queued for the next turn",
  );
});

test("a refused live settings change appears in App errors", async () => {
  const agent: Agent = {
    ...initial,
    status: AgentStatus.running,
    nextTurnSettingsSupported: true,
  };
  const logged = vi.spyOn(appLog, "appendAppLog");
  h.post.mockRejectedValue(new ApiError("Model unavailable", 400));
  render(agent).picker.onModel("gpt-6-luna");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  render(agent);
  render(agent);
  expect(logged).toHaveBeenCalledWith(
    expect.objectContaining({
      level: "error",
      title: "Execution settings change failed",
      reason: "Model unavailable",
    }),
  );
  logged.mockRestore();
});

test("an old rejection cannot fail a newer account's settings change", async () => {
  const logged = vi.spyOn(appLog, "appendAppLog");
  let rejectOld!: (error: Error) => void;
  h.post.mockImplementationOnce(
    () =>
      new Promise<Agent>((_, reject) => {
        rejectOld = reject;
      }),
  );
  h.post.mockImplementationOnce(() => new Promise<Agent>(() => {}));
  const first: Agent = {
    ...initial,
    status: AgentStatus.running,
    nextTurnSettingsSupported: true,
  };
  const second: Agent = { ...first, accountKey: "other" };
  render(first).picker.onModel("gpt-6-luna");
  render(second).picker.onModel("gpt-6-luna");
  rejectOld(new ApiError("Old account refused", 400));
  await Promise.resolve();
  expect(logged).toHaveBeenCalledWith(
    expect.objectContaining({
      reason: "Old account refused",
      detail: expect.stringContaining("account: default"),
    }),
  );
  const modal = render(second).statusModal;
  expect(children(modal).map((node) => node.props.children)).not.toContain(
    "Failed",
  );
  logged.mockRestore();
});

test("a rejected save is logged even after the controller unmounts", async () => {
  let rejectSave!: (error: Error) => void;
  const logged = vi.spyOn(appLog, "appendAppLog");
  h.post.mockImplementationOnce(
    () =>
      new Promise<Agent>((_, reject) => {
        rejectSave = reject;
      }),
  );
  const agent: Agent = {
    ...initial,
    status: AgentStatus.running,
    nextTurnSettingsSupported: true,
  };
  render(agent).picker.onModel("gpt-6-luna");
  for (const cells of h.cells.values())
    for (const cell of cells) cell.cleanup?.();
  rejectSave(new ApiError("Save refused", 400));
  await vi.waitFor(() =>
    expect(logged).toHaveBeenCalledWith(
      expect.objectContaining({ level: "error", reason: "Save refused" }),
    ),
  );
  logged.mockRestore();
});

test("switching picker roles keeps acknowledged worker defaults in the summary", async () => {
  h.read.mockResolvedValue({
    data: [
      {
        model: "gpt-6-astra",
        provider: "codex",
        supportedReasoningEfforts: [{ reasoningEffort: "high" }],
        serviceTiers: [],
      },
    ],
  });
  h.post.mockResolvedValue({
    ...initial,
    workerDefaults: { model: "gpt-6-astra", effort: "high" },
  });
  render().picker.onRole("worker");
  render().picker.onModel("gpt-6-astra");
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledOnce());
  expect(h.read).toHaveBeenCalledExactlyOnceWith({
    query: { account_key: "default", workers: "1", fresh: "1" },
  });
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
      agent: {
        ...initial,
        provider: ProviderKind.claude,
        status,
        inFlight: true,
      },
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
      status: AgentStatus.running,
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
  const lostFullId = required(h.post.mock.calls[0])[1].request_id;
  h.post.mockResolvedValueOnce(ask);
  control(full).props.onChange({ currentTarget: { checked: false } });
  await vi.waitFor(() => expect(control(ask).props.disabled).toBe(false));
  h.post.mockResolvedValueOnce(full);
  control(ask).props.onChange({ currentTarget: { checked: true } });
  await vi.waitFor(() => expect(h.post).toHaveBeenCalledTimes(3));
  expect(required(h.post.mock.calls[2])[1].request_id).not.toBe(lostFullId);
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
      agent: { ...initial, provider: ProviderKind.claude },
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
    expect(required(mutations[1]).request_id).toBe(
      required(mutations[0]).request_id,
    );
    await reopen();
    loseResponse = false;
    await choose(opposite);
    await choose(first);
    expect(required(mutations[2]).request_id).not.toBe(
      required(mutations[0]).request_id,
    );
    expect(required(mutations[3]).request_id).not.toBe(
      required(mutations[0]).request_id,
    );
    expect(required(mutations[3]).request_id).not.toBe(
      required(mutations[2]).request_id,
    );
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
    agent: { ...initial, provider: ProviderKind.claude },
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
  const unresolvedId = required(mutations[1]).request_id;
  expect(unresolvedId).not.toBe(required(mutations[0]).request_id);
  finish({});
  await Promise.resolve();
  control().props.onChange({ currentTarget: { value: "default" } });
  await vi.waitFor(() => expect(mutations).toHaveLength(3));
  expect(required(mutations[2]).request_id).toBe(unresolvedId);
});
