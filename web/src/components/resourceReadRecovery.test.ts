import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Agent } from "../types";

// Execute the real effects and read callbacks without rendering unrelated UI.
const {
  effects,
  cleanups,
  states,
  refs,
  cursor,
  watchers,
  get,
  post,
  syncPost,
  reconcile,
} = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  cleanups: [] as Array<() => void>,
  states: [] as Array<{ value: unknown }>,
  refs: [] as Array<{ current: unknown }>,
  cursor: { state: 0, ref: 0 },
  watchers: new Map<string, () => void>(),
  get: vi.fn(),
  post: vi.fn(),
  syncPost: vi.fn(),
  reconcile: vi.fn(async () => {}),
}));

vi.mock("react", async (importOriginal) => {
  const original = await importOriginal<typeof import("react")>();
  return {
    ...original,
    useCallback: (callback: unknown) => callback,
    useEffect: (effect: () => void | (() => void)) => effects.push(effect),
    useRef: (value: unknown) =>
      refs[cursor.ref++] ?? (refs[cursor.ref - 1] = { current: value }),
    useState: (initial: unknown) => {
      const index = cursor.state++;
      const entry =
        states[index] ??
        (states[index] = {
          value: typeof initial === "function" ? initial() : initial,
        });
      return [
        entry.value,
        (value: unknown) => {
          entry.value =
            typeof value === "function" ? value(entry.value) : value;
        },
      ];
    },
  };
});
vi.mock("../api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api")>()),
  get,
  post,
  syncPost,
}));
vi.mock("../sync/localDraft", () => ({
  updateLocalDraft: async (
    key: string,
    fallback: unknown,
    update: (value: unknown) => unknown,
  ) => {
    const raw = localStorage.getItem(key);
    const result = update(raw === null ? fallback : JSON.parse(raw));
    if (result === null) localStorage.removeItem(key);
    else localStorage.setItem(key, JSON.stringify(result));
    return result;
  },
}));
vi.mock("../sync/send", () => ({ reconcileOutboxReceipts: reconcile }));
vi.mock("../sync/resume", () => ({ onResume: () => () => {} }));
vi.mock("../sync/resourceEvents", () => ({
  watchResourceChanges: (resource: unknown, callback: () => void) => {
    const key = JSON.stringify(resource);
    watchers.set(key, callback);
    return () => watchers.delete(key);
  },
}));

import { useMessageReceipts } from "./useMessageReceipts";
import { useMessageQueue } from "./useMessageQueue";
import { useWorkerModels } from "./agents/WorkerModelPicker";
import { useAccounts } from "./Accounts";
import Usage from "./Usage";
import { ApiError } from "../api";

function mount(action: () => void) {
  cursor.state = 0;
  cursor.ref = 0;
  action();
  for (const effect of effects.splice(0)) {
    const cleanup = effect();
    if (cleanup) cleanups.push(cleanup);
  }
}

function notify(resource: unknown) {
  const callback = watchers.get(JSON.stringify(resource));
  if (!callback) throw new Error("The read callback was not registered");
  callback();
}

describe("resource read callers", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    get.mockReset();
    post.mockReset();
    syncPost.mockReset();
    reconcile.mockClear();
    effects.length = 0;
    states.length = 0;
    refs.length = 0;
    cursor.state = 0;
    cursor.ref = 0;
    watchers.clear();
    const storage = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => storage.set(key, value),
      removeItem: (key: string) => storage.delete(key),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      setInterval,
      clearInterval,
    });
  });

  afterEach(() => {
    for (const cleanup of cleanups.splice(0).reverse()) cleanup();
    expect(post).not.toHaveBeenCalled();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("reconciles the exact delivered message after a lost receipt read", async () => {
    const receipt = { id: "message-one", status: "delivered" };
    get
      .mockRejectedValueOnce(new TypeError("Failed to fetch"))
      .mockResolvedValue({ agent: "lead", items: [receipt] });
    mount(() =>
      useMessageReceipts("lead", "scope", "workspace", ["message-one"]),
    );
    notify({ kind: "receipts", agentId: "lead" });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);
    expect(reconcile).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1_000);
    expect(get).toHaveBeenCalledTimes(2);
    expect(get.mock.calls[1]).toEqual(get.mock.calls[0]);
    expect(reconcile).toHaveBeenCalledExactlyOnceWith(
      "lead",
      [receipt],
      "workspace",
    );
    const value = states[0]!.value as { receipts: Map<string, unknown> };
    expect(value.receipts.get("message-one")).toEqual(receipt);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(get).toHaveBeenCalledTimes(2);
  });

  it("refreshes the accounts list after an accounts resource event", async () => {
    const before = {
      accounts: [{ id: "connected", email: "person@example.test" }],
      archivedAccounts: [],
      defaultAccountKey: "connected",
      logins: [],
    };
    const after = {
      ...before,
      accounts: [
        { id: "connected", email: "person@example.test", disconnected: true },
      ],
    };
    get.mockResolvedValueOnce(before).mockResolvedValueOnce(after);
    mount(() => useAccounts("workspace"));

    notify({ kind: "accounts" });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toEqual(before);
    notify({ kind: "accounts" });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toEqual(after);
    expect(get).toHaveBeenCalledTimes(2);
  });

  it("recovers a deferred queue read without repeating the queue mutation", async () => {
    const old = {
      items: [{ id: "message-one", text: "old" }],
      revision: "v1",
      capabilities: { reorder: true, receipts: true },
    };
    const fresh = { ...old, revision: "v2" };
    const props = {
      id: "lead",
      enabled: true,
      scope: "scope",
      workspaceId: "workspace",
      refresh: vi.fn(async () => {}),
    };
    get.mockResolvedValueOnce(old);
    mount(() => useMessageQueue(props));
    notify({ kind: "queue", agentId: "lead" });
    await vi.advanceTimersByTimeAsync(0);
    cursor.state = 0;
    cursor.ref = 0;
    const queue = useMessageQueue(props);
    effects.length = 0; // This render keeps the already registered effects.
    let finish!: (value: unknown) => void;
    get
      .mockReturnValueOnce(
        new Promise((resolve) => {
          finish = resolve;
        }),
      )
      .mockRejectedValueOnce(new TypeError("Failed to fetch"))
      .mockResolvedValue(fresh);
    syncPost.mockResolvedValue({ status: "updated" });
    const mutation = queue.reorder(["message-one"]);
    await vi.advanceTimersByTimeAsync(0);
    expect(syncPost).toHaveBeenCalledTimes(1);
    expect(get).toHaveBeenCalledTimes(2);
    notify({ kind: "queue", agentId: "lead" });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(2);
    finish(old);
    await mutation;
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(3);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(get).toHaveBeenCalledTimes(4);
    expect(syncPost).toHaveBeenCalledTimes(1);
    expect(syncPost.mock.calls[0]![1]).toMatchObject({
      action: "reorder",
      agent: "lead",
      expected_revision: "v1",
      ordered_ids: ["message-one"],
    });
    expect(typeof syncPost.mock.calls[0]![1].request_id).toBe("string");
    expect(states[0]!.value).toMatchObject({ view: { revision: "v2" } });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(get).toHaveBeenCalledTimes(4);
    expect(syncPost).toHaveBeenCalledTimes(1);
  });

  it("loads models after a transient catalog read without an explicit retry", async () => {
    get
      .mockRejectedValueOnce(new TypeError("Failed to fetch"))
      .mockResolvedValue({ data: [{ model: "gpt-6-luna" }] });
    mount(() => useWorkerModels("default", true, true));
    notify({ kind: "models" });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(get).toHaveBeenCalledTimes(2);
    expect(get.mock.calls[1]).toEqual(get.mock.calls[0]);
    const state = states.find(
      (entry) => (entry.value as { models?: unknown } | null)?.models,
    )?.value;
    expect(state).toMatchObject({
      models: [{ model: "gpt-6-luna" }],
      error: "",
      pending: false,
    });
  });

  it("uses the selected account catalog after an account switch", async () => {
    get.mockImplementation(
      async (_path: string, options: { query: { account_key: string } }) => ({
        data: [{ model: `${options.query.account_key}-model` }],
      }),
    );
    mount(() => useWorkerModels("first-account", true, true));
    notify({ kind: "models" });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "first-account-model" }],
    });

    for (const cleanup of cleanups.splice(0)) cleanup();
    mount(() => useWorkerModels("current-account", true, true));
    notify({ kind: "models" });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "current-account-model" }],
    });
  });

  it("keeps a catalog pending marker under server control", async () => {
    get.mockRejectedValue(
      new ApiError("Pending", 400, { catalogPending: true }),
    );
    mount(() => useWorkerModels("default", true, true));
    notify({ kind: "models" });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(get).toHaveBeenCalledTimes(1);
    const state = states.find(
      (entry) => (entry.value as { models?: unknown } | null)?.models,
    )?.value;
    expect(state).toMatchObject({ models: [], error: "", pending: true });
  });

  it("recovers both cost display reads without repeating a reset-credit action", async () => {
    const calls = new Map<string, number>();
    const session = {
      rootId: "lead",
      pricingState: "ready",
      totalUSD: 0.5,
      unknownModels: [],
      breakdown: {},
    };
    const account = { accountKey: "default", totalUSD: 1 };
    get.mockImplementation(async (path: string) => {
      const count = (calls.get(path) || 0) + 1;
      calls.set(path, count);
      if (count === 1) throw new TypeError("Failed to fetch");
      return path === "/api/session-cost" ? session : account;
    });
    mount(() =>
      Usage({
        agent: { id: "lead", rootId: "lead", accountKey: "default" } as Agent,
        stateDir: "private-fixture",
        limits: null,
        reload: vi.fn(),
      }),
    );
    notify({ kind: "session-cost", agentId: "lead" });
    notify({ kind: "costs" });
    await vi.advanceTimersByTimeAsync(0);
    expect(calls).toEqual(
      new Map([
        ["/api/session-cost", 1],
        ["/api/costs", 1],
      ]),
    );
    await vi.advanceTimersByTimeAsync(1_000);
    expect(calls).toEqual(
      new Map([
        ["/api/session-cost", 2],
        ["/api/costs", 2],
      ]),
    );
    expect(
      states.some(
        (entry) =>
          (entry.value as { value?: unknown } | null)?.value === session,
      ),
    ).toBe(true);
    expect(states.some((entry) => entry.value === account)).toBe(true);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(get).toHaveBeenCalledTimes(4);
  });
});
