import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Agent } from "../types";

// Execute the real effects and read callbacks without rendering unrelated UI.
const {
  effects,
  cleanups,
  states,
  refs,
  memos,
  cursor,
  watchers,
  watchOptions,
  resourceVersions,
  connectionWatchers,
  connectionStream,
  get,
  post,
  syncPost,
  reconcile,
} = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  cleanups: [] as Array<() => void>,
  states: [] as Array<{ value: unknown }>,
  refs: [] as Array<{ current: unknown }>,
  memos: [] as Array<{
    value: unknown;
    dependencies: readonly unknown[] | undefined;
  }>,
  cursor: { state: 0, ref: 0, memo: 0 },
  watchers: new Map<string, (version?: unknown) => void>(),
  watchOptions: new Map<string, { waitForBaseline?: boolean }>(),
  resourceVersions: new Map<string, unknown>(),
  connectionWatchers: new Set<
    (status: string, stream: "stopped" | "connecting" | "open") => void
  >(),
  connectionStream: {
    value: "connecting" as "stopped" | "connecting" | "open",
  },
  get: vi.fn(),
  post: vi.fn(),
  syncPost: vi.fn(),
  reconcile: vi.fn(async () => {}),
}));

vi.mock("react", async (importOriginal) => {
  const original = await importOriginal<typeof import("react")>();
  const scheduleEffect = (
    effect: () => void | (() => void),
    _dependencies?: readonly unknown[],
  ) => effects.push(effect);
  return {
    ...original,
    useCallback: (callback: unknown) => callback,
    // No provider is mounted, so a context read returns its default value.
    useContext: (context: { _currentValue: unknown }) => context._currentValue,
    useMemo: (compute: () => unknown, dependencies?: readonly unknown[]) => {
      const index = cursor.memo++;
      const previous = memos[index];
      if (
        !previous ||
        !dependencies ||
        !previous.dependencies ||
        previous.dependencies.length !== dependencies.length ||
        dependencies.some(
          (value, i) => !Object.is(value, previous.dependencies![i]),
        )
      )
        memos[index] = { value: compute(), dependencies };
      return memos[index]!.value;
    },
    useEffect: scheduleEffect,
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
    useSyncExternalStore: (
      subscribe: (listener: () => void) => () => void,
      getSnapshot: () => unknown,
    ) => {
      const index = cursor.state++;
      const entry = states[index] ?? (states[index] = { value: getSnapshot() });
      entry.value = getSnapshot();
      scheduleEffect(
        () =>
          subscribe(() => {
            entry.value = getSnapshot();
          }),
        [subscribe],
      );
      return entry.value;
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
  watchResourceChanges: (
    resource: unknown,
    callback: (version?: unknown) => void,
    options?: { waitForBaseline?: boolean },
  ) => {
    const key = JSON.stringify(resource);
    watchers.set(key, callback);
    watchOptions.set(key, options || {});
    const known = resourceVersions.get(key);
    if (known) callback(known);
    return () => {
      watchers.delete(key);
      watchOptions.delete(key);
    };
  },
  watchResourceConnection: (
    listener: (
      status: string,
      stream: "stopped" | "connecting" | "open",
    ) => void,
  ) => {
    connectionWatchers.add(listener);
    listener("connecting", connectionStream.value);
    return () => connectionWatchers.delete(listener);
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
  cursor.memo = 0;
  action();
  for (const effect of effects.splice(0)) {
    const cleanup = effect();
    if (cleanup) cleanups.push(cleanup);
  }
}

function notify(resource: unknown, version?: unknown) {
  const callback = watchers.get(JSON.stringify(resource));
  if (!callback) throw new Error("The read callback was not registered");
  callback(version);
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
    memos.length = 0;
    cursor.state = 0;
    cursor.ref = 0;
    cursor.memo = 0;
    watchers.clear();
    watchOptions.clear();
    resourceVersions.clear();
    connectionWatchers.clear();
    connectionStream.value = "connecting";
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
      .mockResolvedValue({
        agent: "lead",
        workspaceId: "workspace",
        items: [receipt],
      });
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

  it("waits for the workspace identity before it reads message receipts", async () => {
    mount(() =>
      useMessageReceipts("lead", "scope", undefined, ["message-one"]),
    );
    expect(watchers.size).toBe(0);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(get).not.toHaveBeenCalled();
    expect(reconcile).not.toHaveBeenCalled();
  });

  it("reads accounts once at the baseline and once per invalidation", async () => {
    const initial = {
      accounts: [{ id: "connected", email: "person@example.test" }],
      archivedAccounts: [],
      defaultAccountKey: "connected",
      logins: [],
    };
    const afterInvalidation = {
      ...initial,
      accounts: [
        { id: "connected", email: "person@example.test", disconnected: true },
      ],
    };
    get.mockResolvedValueOnce(initial).mockResolvedValueOnce(afterInvalidation);
    mount(() => useAccounts("workspace"));
    expect(watchOptions.get(JSON.stringify({ kind: "accounts" }))).toEqual({
      waitForBaseline: true,
    });
    expect(get).not.toHaveBeenCalled();

    notify({ kind: "accounts" }, { epoch: "accounts", revision: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toEqual(initial);
    expect(get).toHaveBeenCalledTimes(1);

    notify({ kind: "accounts" }, { epoch: "accounts", revision: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);

    notify({ kind: "accounts" }, { epoch: "accounts", revision: 2 });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toEqual(afterInvalidation);
    expect(get).toHaveBeenCalledTimes(2);
  });

  it("reads a known baseline once when a watcher subscribes late", async () => {
    resourceVersions.set(JSON.stringify({ kind: "accounts" }), {
      epoch: "accounts",
      revision: 1,
    });
    get.mockResolvedValueOnce({
      accounts: [{ id: "connected" }],
      archivedAccounts: [],
      defaultAccountKey: "connected",
      logins: [],
    });

    mount(() => useAccounts("workspace"));
    await vi.advanceTimersByTimeAsync(0);

    expect(get).toHaveBeenCalledTimes(1);
  });

  it("reads accounts once through the fallback when no stream is available", async () => {
    connectionStream.value = "stopped";
    get.mockResolvedValueOnce({
      accounts: [{ id: "connected" }],
      archivedAccounts: [],
      defaultAccountKey: "connected",
      logins: [],
    });

    mount(() => useAccounts("workspace"));
    await vi.advanceTimersByTimeAsync(0);

    expect(get).toHaveBeenCalledTimes(1);
    expect(get).toHaveBeenCalledWith("/api/accounts");
  });

  it("resubscribes and reads once when the accounts state directory changes", async () => {
    const response = {
      accounts: [{ id: "connected" }],
      archivedAccounts: [],
      defaultAccountKey: "connected",
      logins: [],
    };
    get.mockResolvedValue(response);
    mount(() => useAccounts("workspace-one"));
    notify({ kind: "accounts" }, { epoch: "accounts", revision: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);

    const stopFirstScope = cleanups.pop();
    expect(stopFirstScope).toBeTypeOf("function");
    stopFirstScope!();
    expect(watchers.size).toBe(0);

    states.length = 0;
    mount(() => useAccounts("workspace-two"));
    expect(watchers.size).toBe(1);
    notify({ kind: "accounts" }, { epoch: "accounts", revision: 2 });
    await vi.advanceTimersByTimeAsync(0);

    expect(get).toHaveBeenCalledTimes(2);
    expect(get.mock.calls.map(([path]) => path)).toEqual([
      "/api/accounts",
      "/api/accounts",
    ]);
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
    cursor.memo = 0;
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
    expect(get).not.toHaveBeenCalled();
    notify({ kind: "models" }, { epoch: "catalog", revision: 1 });
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

  it("shares a catalog read and refreshes it after a models resource event", async () => {
    get
      .mockResolvedValueOnce({ data: [{ model: "before-change" }] })
      .mockResolvedValueOnce({ data: [{ model: "after-change" }] });
    mount(() => useWorkerModels("shared-catalog-account", true));
    mount(() => useWorkerModels("shared-catalog-account", true));
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(0);
    expect(watchers.size).toBe(1);

    notify({ kind: "models" }, { epoch: "catalog", revision: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);

    notify({ kind: "models" }, { epoch: "catalog", revision: 2 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(2);
    expect(get.mock.calls[1]?.[0]).toBe("/api/models");
    expect(get.mock.calls[1]?.[1]).toMatchObject({
      query: { account_key: "shared-catalog-account" },
    });
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "after-change" }],
    });
  });

  it("waits for the baseline and rereads once when the catalog changes during its GET", async () => {
    let resolveFirst!: (value: unknown) => void;
    get
      .mockReturnValueOnce(
        new Promise((resolve) => {
          resolveFirst = resolve;
        }),
      )
      .mockResolvedValueOnce({ data: [{ model: "revision-n-plus-one" }] });
    mount(() => useWorkerModels("interleaving-account", true));
    await vi.advanceTimersByTimeAsync(0);
    expect(get).not.toHaveBeenCalled();

    notify({ kind: "models" }, { epoch: "catalog", revision: 10 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);

    notify({ kind: "models" }, { epoch: "catalog", revision: 11 });
    resolveFirst({ data: [{ model: "revision-n" }] });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(2);
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "revision-n-plus-one" }],
    });
  });

  it("does not reread on an unchanged stream reopen and rereads after an epoch change", async () => {
    get
      .mockResolvedValueOnce({ data: [{ model: "epoch-one" }] })
      .mockResolvedValueOnce({ data: [{ model: "epoch-two" }] });
    mount(() => useWorkerModels("epoch-account", true));
    notify({ kind: "models" }, { epoch: "epoch-one", revision: 4 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);

    notify({ kind: "models" }, { epoch: "epoch-one", revision: 4 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);

    notify({ kind: "models" }, { epoch: "epoch-two", revision: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(2);
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "epoch-two" }],
    });
  });

  it.each(["degraded", "offline"] as const)(
    "loads once after a %s stream state and reconciles when its baseline arrives",
    async (status) => {
      get
        .mockResolvedValueOnce({ data: [{ model: "fallback" }] })
        .mockResolvedValueOnce({ data: [{ model: "baseline-current" }] });
      mount(() => useWorkerModels(`fallback-${status}-account`, true));
      for (const listener of connectionWatchers) listener(status, "connecting");
      await vi.advanceTimersByTimeAsync(0);
      expect(get).toHaveBeenCalledTimes(1);

      for (const listener of connectionWatchers) listener(status, "connecting");
      await vi.advanceTimersByTimeAsync(0);
      expect(get).toHaveBeenCalledTimes(1);

      notify({ kind: "models" }, { epoch: "catalog", revision: 3 });
      await vi.advanceTimersByTimeAsync(0);
      expect(get).toHaveBeenCalledTimes(2);
      expect(states[0]?.value).toMatchObject({
        models: [{ model: "baseline-current" }],
      });
    },
  );

  it("reads immediately without a stream and reconciles a later baseline once", async () => {
    connectionStream.value = "stopped";
    get
      .mockResolvedValueOnce({ data: [{ model: "before-baseline" }] })
      .mockResolvedValueOnce({ data: [{ model: "after-baseline" }] });
    mount(() => useWorkerModels("no-stream-account", true));
    expect(watchers.size).toBe(1);
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(1);

    notify({ kind: "models" }, { epoch: "catalog", revision: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(get).toHaveBeenCalledTimes(2);
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "after-baseline" }],
    });
  });

  it("does not start a read when schema mismatch blocks API requests", async () => {
    mount(() => useWorkerModels("schema-mismatch-account", true));
    for (const listener of connectionWatchers)
      listener("schema-mismatch", "stopped");
    await vi.advanceTimersByTimeAsync(0);
    expect(get).not.toHaveBeenCalled();
    expect(watchers.size).toBe(1);
  });

  it("uses the selected account catalog after an account switch", async () => {
    get.mockImplementation(
      async (_path: string, options: { query: { account_key: string } }) => ({
        data: [{ model: `${options.query.account_key}-model` }],
      }),
    );
    mount(() => useWorkerModels("first-account", true, true));
    notify({ kind: "models" }, { epoch: "catalog", revision: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "first-account-model" }],
    });

    for (const cleanup of cleanups.splice(0)) cleanup();
    mount(() => useWorkerModels("current-account", true, true));
    notify({ kind: "models" }, { epoch: "catalog", revision: 2 });
    await vi.advanceTimersByTimeAsync(0);
    expect(states[0]?.value).toMatchObject({
      models: [{ model: "current-account-model" }],
    });
  });

  it("keeps a catalog pending marker under server control", async () => {
    get.mockRejectedValue(
      new ApiError("Pending", 400, { catalogPending: true }),
    );
    mount(() => useWorkerModels("pending-account", true, true));
    notify({ kind: "models" }, { epoch: "catalog", revision: 1 });
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
