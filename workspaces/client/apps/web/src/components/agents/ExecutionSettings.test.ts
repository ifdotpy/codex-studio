import { createElement, type ComponentProps } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { beforeEach, expect, test, vi } from "vitest";
import { ExecutionSettings } from "./ExecutionSettings";
import type { AgentSetupPickerProps } from "./AgentSetupPicker";

const choices = vi.hoisted(() => ({
  pickers: [] as AgentSetupPickerProps[],
  states: [] as unknown[],
  terminal: undefined as (() => void) | undefined,
  read: vi.fn(),
  post: vi.fn(),
}));

vi.mock("react", async (load) => {
  const actual = await load<typeof import("react")>();
  return {
    ...actual,
    // Server rendering exposes the real callbacks without a browser. Record
    // their state writes as a completion barrier, not a retry or elapsed wait.
    useState: (initial: unknown) => {
      const [value, write] = actual.useState(initial);
      return [
        value,
        (next: unknown) => {
          choices.states.push(next);
          write(next);
          if (next === false || (typeof next === "string" && next))
            choices.terminal?.();
        },
      ];
    },
  };
});

vi.mock("./AgentSetupPicker", async (load) => {
  const actual = await load<typeof import("./AgentSetupPicker")>();
  return {
    ...actual,
    AgentSetupPicker: (props: AgentSetupPickerProps) => {
      choices.pickers.push(props);
      return createElement(actual.AgentSetupPicker, props);
    },
  };
});

vi.mock("./WorkerModelPicker", async (load) => ({
  ...(await load<typeof import("./WorkerModelPicker")>()),
  useModelCatalogReader: () => ({
    scope: "future-worker-account",
    read: choices.read,
    watchResourceReads: vi.fn(),
  }),
  // The ordinary display retains stale rows. The action must use the real
  // readWorkerModels preflight instead of treating this display as admission.
  useWorkerModels: () => ({
    models: [
      {
        model: "gpt-6.1-sol",
        provider: "codex",
        serviceTiers: [],
        supportedReasoningEfforts: [{ reasoningEffort: "medium" }],
      },
      {
        model: "gpt-6-luna",
        provider: "codex",
        serviceTiers: [],
        supportedReasoningEfforts: [{ reasoningEffort: "medium" }],
      },
    ],
    loading: false,
    error: "",
    retry: vi.fn(),
  }),
}));

vi.mock("../../servers/ServerServicesContext", async (load) => ({
  ...(await load<typeof import("../../servers/ServerServicesContext")>()),
  useServerApi: () => ({
    post: choices.post,
    saved: (_key: string, fallback: unknown) => fallback,
    save: vi.fn(),
  }),
  useServerServices: () => null,
}));

beforeEach(() => {
  choices.pickers.length = 0;
  choices.states.length = 0;
  choices.terminal = undefined;
  choices.read.mockReset();
  choices.post.mockReset();
});

vi.mock("../models/modelWhitelist", async (load) => ({
  ...(await load<typeof import("../models/modelWhitelist")>()),
  useModelWhitelist: () => null,
}));

for (const isLead of [true, false]) {
  test(`the ${isLead ? "lead" : "worker"} panel always renders every role`, () => {
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        null,
        createElement(ExecutionSettings, {
          presentation: "panel",
          settingsRow: true,
          agent: {
            id: "picker-test",
            name: "Test",
            isLead,
            source: "managed",
            model: "gpt-6-astra",
            status: "idle",
          },
          catalog: { models: [], loading: false, error: "", retry: () => {} },
          refresh: async () => {},
        }),
      ),
    );
    for (const label of ["Lead", "Workers", "Review"]) {
      expect(markup).toContain(`>${label}</span></label>`);
    }
    expect(markup.match(/aria-label="Agent role"/g)).toHaveLength(1);
  });
}

for (const initialRole of ["orchestrator", "review"] as const) {
  test(`a worker without its lead keeps ${initialRole} visible as unavailable`, () => {
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        null,
        createElement(ExecutionSettings, {
          presentation: "panel",
          initialRole,
          agent: {
            id: "worker",
            name: "Worker",
            isLead: false,
            source: "managed",
            model: "gpt-6-luna",
            status: "idle",
          },
          catalog: { models: [], loading: false, error: "", retry: () => {} },
          refresh: async () => {},
        }),
      ),
    );
    expect(markup).toContain(
      "Managed by the lead. Its settings are unavailable in this chat.",
    );
    expect(markup).toContain("Inherited settings unavailable");
    expect(markup).toContain('aria-disabled="true"');
    expect(markup).not.toContain('data-value="gpt-6-luna"');
    for (const label of ["Lead", "Workers", "Review"])
      expect(markup).toContain(`>${label}</span></label>`);
  });
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

const freshModels = {
  data: [
    {
      model: "gpt-6.1-sol",
      provider: "codex",
      isDefault: true,
      defaultReasoningEffort: "medium",
      supportedReasoningEfforts: [{ reasoningEffort: "medium" }],
      serviceTiers: [],
    },
    {
      model: "gpt-6-luna",
      provider: "codex",
      supportedReasoningEfforts: [{ reasoningEffort: "medium" }],
      serviceTiers: [],
    },
  ],
};

function futureWorkerChoice(automatic = false) {
  const agent = {
    id: "pm-installation",
    rootId: "pm-installation",
    name: "PM installation",
    accountKey: "lead-claude",
    provider: "claude",
    model: "default",
    isLead: true,
    source: "managed",
    status: "waiting",
    workerDefaults: {
      accountKey: automatic ? undefined : "old-codex",
      model: "gpt-6.1-sol",
      effort: "medium",
      fastMode: false,
      daybreakEnabled: false,
      delegationDepth: 1,
    },
  } satisfies ComponentProps<typeof ExecutionSettings>["agent"];
  const children = [
    { id: "existing-child", rootId: agent.id, accountKey: "old-codex" },
  ];
  const originalChildren = structuredClone(children);
  const done = deferred<void>();
  choices.terminal = () => done.resolve();
  choices.post.mockImplementation(async (path, request) => {
    expect(path).toBe("/api/conversation");
    expect(request.expected_account_key).toBe("lead-claude");
    return {
      ...agent,
      workerDefaults: {
        ...agent.workerDefaults,
        accountKey: request.worker_defaults.account_key,
      },
    };
  });
  renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ExecutionSettings, {
        presentation: "panel",
        initialRole: "worker",
        settingsRow: true,
        agent,
        team: children,
        accounts: [
          {
            id: "lead-claude",
            label: "Lead",
            home: "/fixture/lead-claude",
            source: "fixture",
            provider: "claude",
            status: "ready",
          },
          {
            id: "old-codex",
            label: "Old",
            home: "/fixture/old-codex",
            source: "fixture",
            provider: "codex",
            status: "ready",
          },
          {
            id: "future-codex",
            label: "Future",
            home: "/fixture/future-codex",
            source: "fixture",
            provider: "codex",
            status: "ready",
          },
        ],
        catalog: { models: [], loading: false, error: "", retry: vi.fn() },
        refresh: async () => {},
      }),
    ),
  );
  expect(choices.pickers).toHaveLength(1);
  return {
    picker: choices.pickers[0],
    agent,
    children,
    originalChildren,
    done,
  };
}

test.each(["future-codex", null])(
  "the actual future-worker account choice waits for fresh metadata: %s",
  async (target) => {
    const gate = deferred<typeof freshModels>();
    choices.read.mockReturnValue(gate.promise);
    const fixture = futureWorkerChoice();
    fixture.picker.onAccount(target);
    try {
      expect(choices.read).toHaveBeenCalledExactlyOnceWith({
        query: {
          account_key: target || "lead-claude",
          workers: target ? undefined : "1",
          fresh: "1",
        },
      });
      expect(choices.states).toContain(true);
      expect(choices.post).not.toHaveBeenCalled();
      expect(fixture.agent.workerDefaults.accountKey).toBe("old-codex");
      expect(fixture.children).toEqual(fixture.originalChildren);
    } finally {
      gate.resolve(freshModels);
      await fixture.done.promise;
    }
    expect(choices.post).toHaveBeenCalledExactlyOnceWith(
      "/api/conversation",
      {
        id: "pm-installation",
        expected_account_key: "lead-claude",
        worker_defaults: {
          account_key: target,
          model: "gpt-6.1-sol",
          effort: "medium",
          fast_mode: false,
          daybreak_enabled: false,
          delegation_depth: 1,
        },
      },
      { timeoutMs: 15000 },
    );
    expect(fixture.children).toEqual(fixture.originalChildren);
  },
);

test("the actual future-worker account preflight blocks duplicate choices", async () => {
  const gate = deferred<typeof freshModels>();
  choices.read.mockReturnValue(gate.promise);
  const fixture = futureWorkerChoice();
  fixture.picker.onAccount("future-codex");
  fixture.picker.onAccount("future-codex");
  try {
    expect(choices.read).toHaveBeenCalledOnce();
    expect(choices.post).not.toHaveBeenCalled();
  } finally {
    gate.resolve(freshModels);
    await fixture.done.promise;
  }
  expect(choices.post).toHaveBeenCalledOnce();
});

test.each([false, true])(
  "the actual future-worker model choice waits for fresh metadata with automatic=%s",
  async (automatic) => {
    const gate = deferred<typeof freshModels>();
    choices.read.mockReturnValue(gate.promise);
    const fixture = futureWorkerChoice(automatic);
    fixture.picker.onModel("gpt-6-luna");
    fixture.picker.onAccount("future-codex");
    try {
      expect(choices.read).toHaveBeenCalledExactlyOnceWith({
        query: {
          account_key: automatic ? "lead-claude" : "old-codex",
          workers: automatic ? "1" : undefined,
          fresh: "1",
        },
      });
      expect(choices.post).not.toHaveBeenCalled();
      expect(fixture.children).toEqual(fixture.originalChildren);
    } finally {
      gate.resolve(freshModels);
      await fixture.done.promise;
    }
    expect(choices.post).toHaveBeenCalledOnce();
    expect(choices.post.mock.calls[0]).toEqual([
      "/api/conversation",
      {
        id: "pm-installation",
        expected_account_key: "lead-claude",
        worker_defaults: {
          account_key: automatic ? null : "old-codex",
          model: "gpt-6-luna",
          effort: "medium",
          fast_mode: false,
          daybreak_enabled: false,
          delegation_depth: 1,
        },
      },
      { timeoutMs: 15000 },
    ]);
    expect(fixture.children).toEqual(fixture.originalChildren);
  },
);

test("a fresh catalog cannot silently replace the requested future-worker model", async () => {
  const gate = deferred<typeof freshModels>();
  choices.read.mockReturnValue(gate.promise);
  const fixture = futureWorkerChoice();
  fixture.picker.onModel("gpt-6-luna");
  gate.resolve({ data: [freshModels.data[0]] });
  await fixture.done.promise;
  expect(choices.post).not.toHaveBeenCalled();
  expect(choices.states).toContain(
    "This model is not available for the selected worker accounts.",
  );
  expect(fixture.agent.workerDefaults.model).toBe("gpt-6.1-sol");
  expect(fixture.children).toEqual(fixture.originalChildren);
});

test.each([
  new Error("The model catalog is unavailable."),
  new DOMException("The account lookup was cancelled.", "AbortError"),
])(
  "a failed account preflight preserves the choice and releases its lock: %s",
  async (failure) => {
    const gate = deferred<typeof freshModels>();
    choices.read.mockReturnValue(gate.promise);
    const fixture = futureWorkerChoice();
    fixture.picker.onAccount("future-codex");
    gate.reject(failure);
    await fixture.done.promise;
    expect(choices.post).not.toHaveBeenCalled();
    expect(choices.states).toContain(failure.message);
    expect(fixture.agent.workerDefaults.accountKey).toBe("old-codex");
    expect(fixture.children).toEqual(fixture.originalChildren);

    const retry = deferred<void>();
    choices.terminal = () => retry.resolve();
    choices.read.mockResolvedValue(freshModels);
    fixture.picker.onAccount("future-codex");
    await retry.promise;
    expect(choices.read).toHaveBeenCalledTimes(2);
    expect(choices.post).toHaveBeenCalledOnce();
    expect(fixture.children).toEqual(fixture.originalChildren);
  },
);
