import type { ComponentHookCell } from "./componentTestFixtures";
import type { EffectCallback, PropsWithChildren } from "react";
import assert from "node:assert/strict";
import { required } from "./componentTestFixtures";
import { afterEach, expect, it, vi } from "vitest";
import {
  isValidElement,
  createElement,
  type ReactElement,
  type ReactNode,
} from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { Agent, Message } from "../types";
import type { Account } from "./Accounts";

const h = vi.hoisted(() => ({
  cursor: 0,
  cells: [] as ComponentHookCell[],
  effects: [] as Array<() => void>,
  get: vi.fn(),
  post: vi.fn(),
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => {
    const index = h.cursor++;
    h.cells[index] ??= {
      value: typeof initial === "function" ? initial() : initial,
    };
    return [
      required(h.cells[index]).value,
      (value: unknown) => {
        required(h.cells[index]).value =
          typeof value === "function"
            ? value(required(h.cells[index]).value)
            : value;
      },
    ];
  },
  useRef: (initial: unknown) => {
    const index = h.cursor++;
    h.cells[index] ??= { current: initial };
    return h.cells[index];
  },
  useMemo: (compute: () => unknown) => compute(),
  useCallback: (callback: unknown) => callback,
  useEffect: (effect: EffectCallback, deps: unknown[]) => {
    const index = h.cursor++;
    const previous = h.cells[index];
    if (
      !previous ||
      deps.some((value, i) => !Object.is(value, previous.deps?.[i]))
    ) {
      previous?.cleanup?.();
      h.cells[index] = { deps };
      h.effects.push(() => {
        required(h.cells[index]).cleanup = effect() || undefined;
      });
    }
  },
}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ get: h.get, post: h.post, saved: () => null }),
  useServerStorage: () => ({}),
  useServerSessionCosts: () => ({ peekSessionCost: () => null }),
}));
vi.mock("./agents/WorkerModelPicker", () => ({
  useWorkerModels: () => ({ models: [], loading: false }),
}));
vi.mock("./ClaudeFileBackups", () => ({ default: "file-backups" }));
vi.mock("./AccountSignInNotice", () => ({ default: "sign-in" }));
vi.mock("./ProjectDirectoryPicker", () => ({ default: "directory-picker" }));

import { useTurnErrors } from "./useTurnErrors";
import MessageInfo from "./conversation/transcript/MessageInfo";
import { ClaudeSettings } from "./ClaudeSettings";
import ClaudeRepairNotice from "./ClaudeRepairNotice";
import ProjectLocationsSettings from "./ProjectLocationsSettings";
import { NativeNotice } from "./NativeNotice";

function render<T>(read: () => T) {
  h.cursor = 0;
  const value = read();
  for (const effect of h.effects.splice(0)) effect();
  return value;
}
async function settle() {
  for (let i = 0; i < 8; i++) await Promise.resolve();
}
function find(
  node: ReactNode,
  predicate: (
    node: ReactElement<PropsWithChildren<Record<string, unknown>>>,
  ) => boolean,
): ReactElement<PropsWithChildren<Record<string, unknown>>> | undefined {
  if (Array.isArray(node)) {
    for (const child of node) {
      const match = find(child, predicate);
      if (match) return match;
    }
  } else if (isValidElement<PropsWithChildren<Record<string, unknown>>>(node)) {
    const element = node;
    return predicate(element)
      ? element
      : find(element.props.children, predicate);
  }
}
afterEach(() => {
  for (const cell of h.cells) cell?.cleanup?.();
  h.cells = [];
  h.effects = [];
  h.get.mockReset();
  h.post.mockReset();
});

const agent: Agent = {
  id: "detail-agent",
  threadId: "thread",
  accountKey: "claude",
  epoch: 1,
};

it("new failed and uncertain turns use inline errors without analytics reads", async () => {
  h.get.mockResolvedValue({ turns: [] });
  render(() => useTurnErrors([], agent, "live-turns"));
  await settle();
  for (const turnStatus of ["failed", "uncertain"] as const) {
    const items: Message[] = [
      {
        id: turnStatus,
        role: "assistant",
        text: "answer",
        turnId: turnStatus,
        turnStatus,
      },
    ];
    const result = render(() => useTurnErrors(items, agent, "live-turns"));
    await settle();
    expect(result.loading.size).toBe(0);
    expect(h.get).not.toHaveBeenCalled();
    const inline = render(() =>
      useTurnErrors(
        [{ ...required(items[0]), turnError: "Streamed failure" }],
        agent,
        "live-turns",
      ),
    );
    expect(
      inline.items.some(
        (item) =>
          item.nativeError === "Streamed failure" ||
          item.turnError === "Streamed failure",
      ),
    ).toBe(true);
    if (turnStatus === "failed") {
      const markup = renderToStaticMarkup(
        createElement(NativeNotice, { item: inline.items.at(-1)! }),
      );
      expect(markup).toContain("Streamed failure");
      expect(h.get).not.toHaveBeenCalled();
    }
  }
});

it("historical error first load and an explicit retry still read once each", async () => {
  h.get
    .mockRejectedValueOnce(new Error("unavailable"))
    .mockResolvedValue({ turns: [] });
  const items: Message[] = [
    {
      id: "past",
      role: "assistant",
      text: "",
      turnId: "past",
      turnStatus: "failed",
    },
  ];
  const read = () => useTurnErrors(items, agent, "past-turns");
  render(read);
  await settle();
  const result = render(read);
  expect(result.loading.size).toBe(0);
  expect(result.failed.has("past")).toBe(true);
  result.retry("past");
  render(read);
  await settle();
  expect(h.get).toHaveBeenCalledTimes(2);
});

it("opening a different thread loads its historical errors once", async () => {
  h.get.mockResolvedValue({ turns: [] });
  const items: Message[] = [
    {
      id: "old",
      role: "assistant",
      text: "",
      turnId: "old",
      turnStatus: "failed",
    },
  ];
  render(() => useTurnErrors(items, agent, "thread-switch"));
  await settle();
  const nextAgent = { ...agent, threadId: "next-thread" };
  render(() => useTurnErrors(items, nextAgent, "thread-switch"));
  await settle();
  expect(h.get).toHaveBeenCalledTimes(2);
  expect(required(required(h.get.mock.calls[1])[1]).query.thread).toBe(
    "next-thread",
  );
  render(() =>
    useTurnErrors(
      [...items, { ...required(items[0]), id: "live", turnId: "live" }],
      nextAgent,
      "thread-switch",
    ),
  );
  await settle();
  expect(h.get).toHaveBeenCalledTimes(2);
});

it("message info reads on open and reopen but not streamed turn metadata changes", async () => {
  h.get.mockResolvedValue({ model: "model" });
  let message: Message = {
    id: "item",
    role: "assistant",
    text: "answer",
    streaming: true,
  };
  const read = () =>
    MessageInfo({
      message,
      agentId: agent.id,
      stateDir: "fixture",
      onCopy: vi.fn(),
    });
  let view = render(read);
  expect(h.get).not.toHaveBeenCalled();
  view.props.onChange(true);
  render(read);
  await settle();
  message = {
    ...message,
    turnId: "turn",
    threadId: "thread",
    streaming: false,
    turnStatus: "completed",
  };
  view = render(read);
  await settle();
  expect(h.get).toHaveBeenCalledTimes(1);
  view.props.onChange(false);
  view = render(read);
  view.props.onChange(true);
  render(read);
  await settle();
  expect(h.get).toHaveBeenCalledTimes(2);
});

it("Claude account status and key updates do not reload settings; explicit retry does", async () => {
  h.post.mockRejectedValue(new Error("Sign in with claude auth login first"));
  let currentAgent = agent;
  const accountFields = {
    id: "claude",
    status: "signedOut",
    home: "/fixture",
    source: "fixture",
    provider: "claude",
    label: "Claude",
  } satisfies Account;
  let account: Omit<typeof accountFields, "status"> & Pick<Account, "status"> =
    accountFields;
  const read = () => ClaudeSettings({ agent: currentAgent, account });
  render(read);
  await settle();
  account = { ...account, status: "ready" };
  currentAgent = { ...agent, accountKey: "other" };
  const view = render(read);
  await settle();
  expect(h.post).toHaveBeenCalledTimes(1);
  const retry = find(
    view,
    (node) => node.props.children === "Retry Claude settings",
  );
  expect(retry).toBeDefined();
  h.post.mockResolvedValue({
    settings: { permissionMode: "acceptEdits" },
    turns: [],
    tasks: [],
  });
  assert.ok(typeof retry?.props.onClick === "function");
  retry.props.onClick();
  render(read);
  await settle();
  expect(h.post).toHaveBeenCalledTimes(2);
  expect(render(read).props["aria-busy"]).toBe(false);
  expect(
    find(render(read), (node) => node.props.label === "Permission mode")!.props
      .value,
  ).toBe("acceptEdits");
  expect(
    find(
      render(read),
      (node) => node.props.children === "Retry Claude settings",
    ),
  ).toBeUndefined();
});

it("repair scope changes hide old evidence without reads; refresh uses the current agent", async () => {
  h.get.mockResolvedValue({ status: "none" });
  let currentAgent = { ...agent, error: "CLAUDE_SESSION_REPAIR_REQUIRED" };
  const read = () => ClaudeRepairNotice({ agent: currentAgent });
  render(read);
  await settle();
  for (const patch of [
    { epoch: 2 },
    { accountKey: "other" },
    { threadId: "new-thread" },
  ]) {
    currentAgent = { ...currentAgent, ...patch };
    render(read);
    await settle();
    expect(h.get).toHaveBeenCalledTimes(1);
  }
  const refresh = find(
    render(read),
    (node) => node.props.children === "Refresh saved report",
  );
  assert.ok(typeof refresh?.props.onClick === "function");
  refresh.props.onClick();
  render(read);
  await settle();
  expect(h.get).toHaveBeenCalledTimes(2);
});

it("location array replacements, additions and path changes do not refetch folder info", async () => {
  h.get.mockResolvedValue({ gitHead: "initial-head" });
  const location = { serverId: "local", path: "/one", projectId: "project" };
  let project = {
    id: "project",
    name: "Project",
    path: "/one",
    locations: [location],
  };
  const read = () => ProjectLocationsSettings({ project, saved: vi.fn() });
  render(read);
  await settle();
  expect(h.get).toHaveBeenCalledTimes(1);
  for (const locations of [
    [{ ...location }],
    [{ ...location, path: "/two" }],
    [location, { ...location, serverId: "remote" }],
  ]) {
    project = { ...project, locations };
    render(read);
    await settle();
    expect(h.get).toHaveBeenCalledTimes(1);
  }
});
