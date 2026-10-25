import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Agent, Snapshot } from "../types";

// Execute the real effects and read callbacks without rendering unrelated UI.
const {
  projectionListeners,
  contexts,
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
  subscription,
} = vi.hoisted(() => ({
  projectionListeners: new Set<(snapshot: unknown) => void>(),
  contexts: new Map<unknown, unknown>(),
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
  reconcile: vi.fn(async (..._args: unknown[]) => {}),
  subscription: vi.fn(() => ({
    data: undefined,
    status: "loading",
    error: null,
  })),
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
    useContext: (context: { _currentValue: unknown }) =>
      contexts.has(context) ? contexts.get(context) : context._currentValue,
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
vi.mock("../sync/localDraft", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../sync/localDraft")>()),
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
vi.mock("../sync/subscriptions", () => ({
  useSubscription: subscription,
}));
vi.mock("../sync/subscriptions/events", () => ({
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

vi.mock("../sync/client", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../sync/client")>()),
  subscribeStateProjection: (accept: (snapshot: unknown) => void) => {
    projectionListeners.add(accept);
    return () => projectionListeners.delete(accept);
  },
}));

function receiveReceipts(receipts: unknown[]) {
  for (const accept of projectionListeners)
    accept({ runtime: { messageReceipts: receipts } });
}

import { useMessageReceipts } from "./useMessageReceipts";
import { useMessageQueue } from "./useMessageQueue";
import {
  useWorkerModels,
  readWorkerModels,
  ModelCatalogReader,
} from "./agents/WorkerModelPicker";
import { useAccounts } from "./Accounts";
import Usage from "./Usage";

function mount<T>(action: () => T): T {
  cursor.state = 0;
  cursor.ref = 0;
  cursor.memo = 0;
  const result = action();
  for (const effect of effects.splice(0)) {
    const cleanup = effect();
    if (cleanup) cleanups.push(cleanup);
  }
  return result;
}

describe("resource read callers", () => {
  beforeEach(() => {
    projectionListeners.clear();
    vi.useFakeTimers();
    get.mockReset();
    post.mockReset();
    syncPost.mockReset();
    reconcile.mockClear();
    subscription.mockClear();
    contexts.clear();
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

  it("reconciles requested receipt entities with zero idle GETs", async () => {
    const receipt = { id: "message-one", agent: "lead", status: "delivered" };
    mount(() =>
      useMessageReceipts("lead", "scope", "workspace", ["message-one"]),
    );
    receiveReceipts([
      receipt,
      { ...receipt, id: "unrequested" },
      { ...receipt, agent: "other" },
    ]);
    await vi.advanceTimersByTimeAsync(0);
    expect(reconcile).toHaveBeenCalledExactlyOnceWith(
      "lead",
      [receipt],
      "workspace",
      expect.any(AbortSignal),
    );
    expect(
      (states[0]!.value as { receipts: Map<string, unknown> }).receipts.get(
        receipt.id,
      ),
    ).toEqual(receipt);
    receiveReceipts([
      receipt,
      { ...receipt, agent: "other", id: "other-message" },
    ]);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(reconcile).toHaveBeenCalledTimes(1);
    expect(get).not.toHaveBeenCalled();
  });

  it("persists delivery before publishing a receipt that can retire its reader", async () => {
    const receipt = { id: "message-one", agent: "lead", status: "delivered" };
    let finishWrite: (() => void) | undefined;
    reconcile.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          finishWrite = resolve;
        }),
    );
    mount(() =>
      useMessageReceipts("lead", "scope", "workspace", ["message-one"]),
    );
    receiveReceipts([receipt]);
    await vi.advanceTimersByTimeAsync(0);

    expect(reconcile).toHaveBeenCalledTimes(1);
    const currentReceipts = () =>
      (states[0]!.value as { receipts: Map<string, unknown> }).receipts;
    expect(currentReceipts().has("message-one")).toBe(false);
    finishWrite!();
    await vi.advanceTimersByTimeAsync(0);
    expect(currentReceipts().get("message-one")).toEqual(receipt);
    cleanups.pop()?.();
    const signal = reconcile.mock.calls[0]?.[3] as AbortSignal;
    expect(signal.aborted).toBe(true);
  });

  it("discards a queued receipt projection after unmount", async () => {
    mount(() =>
      useMessageReceipts("lead", "scope", "workspace", ["message-one"]),
    );
    receiveReceipts([
      { id: "message-one", agent: "lead", status: "delivered" },
    ]);
    const previous = states[0]!.value;
    cleanups.pop()?.();
    await vi.advanceTimersByTimeAsync(0);
    expect(get).not.toHaveBeenCalled();
    expect(reconcile).not.toHaveBeenCalled();
    expect(states[0]!.value).toBe(previous);
  });

  it("aborts reconciliation when receipt readers unmount", async () => {
    const receipt = { id: "message-one", agent: "lead", status: "delivered" };
    reconcile.mockImplementationOnce(
      (_room, _receipts, _workspaceId, value) => {
        const signal = value as AbortSignal;
        return new Promise<void>((resolve) =>
          signal.addEventListener("abort", () => resolve(), { once: true }),
        );
      },
    );
    mount(() =>
      useMessageReceipts("lead", "scope", "workspace", ["message-one"]),
    );
    receiveReceipts([receipt]);
    await vi.advanceTimersByTimeAsync(0);

    expect(reconcile).toHaveBeenCalledTimes(1);
    const signal = reconcile.mock.calls[0]?.[3] as AbortSignal;
    expect(signal.aborted).toBe(false);
    cleanups.pop()?.();

    expect(signal.aborted).toBe(true);
    await vi.advanceTimersByTimeAsync(0);
    expect(get).not.toHaveBeenCalled();
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

  it("applies safe account summaries without baseline or update GETs", async () => {
    mount(() => useAccounts("workspace"));
    const initial = {
      accounts: [{ id: "connected", email: "person@example.test" }],
      archivedAccounts: [],
      defaultAccountKey: "connected",
      defaultAccountGroups: [],
      supportsDelete: true,
      supportsDisconnect: true,
    };
    for (const accept of projectionListeners)
      accept({ runtime: { accountsSummary: initial } });
    expect(states[0]?.value).toEqual({ ...initial, logins: [] });
    const changed = {
      ...initial,
      accounts: [{ id: "connected", disconnected: true }],
    };
    for (const accept of projectionListeners)
      accept({ runtime: { accountsSummary: changed } });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(states[0]?.value).toEqual({ ...changed, logins: [] });
    expect(get).not.toHaveBeenCalled();
    expect(watchers.size).toBe(0);
  });

  it("keeps explicit account recovery reads available", async () => {
    const response = {
      accounts: [],
      archivedAccounts: [],
      defaultAccountKey: "default",
      logins: [],
    };
    get.mockResolvedValue(response);
    let accounts!: ReturnType<typeof useAccounts>;
    mount(() => {
      accounts = useAccounts("workspace");
    });
    await accounts.refresh();
    expect(get).toHaveBeenCalledExactlyOnceWith("/api/accounts");
    expect(states[0]?.value).toEqual(response);
  });

  it("applies queue projections without GETs at baseline, update, and idle", async () => {
    const props = {
      id: "lead",
      enabled: true,
      scope: "scope",
      workspaceId: "workspace",
      refresh: vi.fn(async () => {}),
    };
    mount(() => useMessageQueue(props));
    const publish = (revision: string, text: string) => {
      for (const accept of projectionListeners)
        accept({
          runtime: {
            queuesByAgent: {
              lead: {
                items: [{ id: "message-one", text }],
                revision,
                capabilities: { reorder: true, receipts: true },
              },
            },
          },
        });
    };
    publish("v1", "old");
    expect(
      watchers.has(JSON.stringify({ kind: "queue", agentId: "lead" })),
    ).toBe(false);
    await vi.advanceTimersByTimeAsync(0);
    expect(get).not.toHaveBeenCalled();
    expect(states[0]!.value).toMatchObject({
      view: { revision: "v1", items: [{ text: "old" }] },
    });
    publish("v2", "new");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(states[0]!.value).toMatchObject({
      view: { revision: "v2", items: [{ text: "new" }] },
    });
    expect(get).not.toHaveBeenCalled();
    expect(syncPost).not.toHaveBeenCalled();
  });

  it("projects account and worker model lists from streamed model entities without GETs", async () => {
    const row = (
      accountKey: string,
      model: string,
      hidden: boolean,
      catalogPosition: number,
    ) => ({
      accountKey,
      model,
      hidden,
      provider: accountKey === "default" ? "codex" : "claude",
      catalogPosition,
      serviceTiers: [],
      supportedReasoningEfforts: [],
    });
    const snapshot = (
      modelsByAccount: NonNullable<Snapshot["runtime"]["modelsByAccount"]>,
    ) => ({
      runtime: { modelsByAccount },
      entitiesComplete: true,
    });
    let current = snapshot({
      default: [
        row("default", "shared-model", false, 0),
        row("default", "hidden-model", true, 1),
      ],
      secondary: [
        row("secondary", "other-model", false, 0),
        row("secondary", "shared-model", false, 1),
      ],
    });
    const listeners: Array<(value: typeof current) => void> = [];
    const reader = {
      scope: "model-entities",
      cacheOwner: {},
      read: get,
      subscribeState: (
        accept: (value: typeof current) => void,
        _fail: (error: unknown | null) => void,
      ) => {
        listeners.push(accept);
        accept(current);
        return vi.fn();
      },
    };
    contexts.set(ModelCatalogReader, reader);

    mount(() => useWorkerModels("default", true));
    const accountView = mount(() => useWorkerModels("default", true));
    expect(accountView.models).toMatchObject([{ model: "shared-model" }]);
    expect(states[0]!.value).toMatchObject({
      models: [{ model: "shared-model" }, { model: "hidden-model" }],
      pending: false,
      loaded: true,
    });
    expect(get).not.toHaveBeenCalled();

    current = snapshot({
      default: [row("default", "updated-model", false, 0)],
      secondary: [row("secondary", "other-model", false, 0)],
    });
    listeners[0]!(current);
    const updatedAccountView = mount(() => useWorkerModels("default", true));
    expect(updatedAccountView.models).toMatchObject([
      { model: "updated-model" },
    ]);
    expect(states[0]!.value).toMatchObject({
      models: [{ model: "updated-model" }],
    });
    expect(get).not.toHaveBeenCalled();

    cursor.state = 0;
    cursor.ref = 0;
    cursor.memo = 0;
    states.length = 0;
    memos.length = 0;
    contexts.set(ModelCatalogReader, reader);
    mount(() => useWorkerModels("default", true, true));
    const workerView = mount(() => useWorkerModels("default", true, true));
    expect(workerView.models).toMatchObject([
      { model: "updated-model" },
      { model: "other-model" },
    ]);
    await expect(
      readWorkerModels(reader, "default", true),
    ).resolves.toMatchObject([
      { model: "updated-model" },
      { model: "other-model" },
    ]);
    expect(states[0]!.value).toMatchObject({ pending: false });
    expect(get).not.toHaveBeenCalled();
  });

  it("keeps the model picker loading until the streamed entity baseline completes", () => {
    let snapshot: {
      runtime: {
        modelsByAccount: NonNullable<Snapshot["runtime"]["modelsByAccount"]>;
      };
      entitiesComplete?: boolean;
    } = { runtime: { modelsByAccount: {} }, entitiesComplete: false };
    let acceptSnapshot!: (value: typeof snapshot) => void;
    const reader = {
      scope: "pending-model-entities",
      cacheOwner: {},
      read: get,
      subscribeState: (accept: typeof acceptSnapshot) => {
        acceptSnapshot = accept;
        accept(snapshot);
        return vi.fn();
      },
    };
    contexts.set(ModelCatalogReader, reader);
    const pendingView = mount(() => useWorkerModels("default", true));
    expect(pendingView.loading).toBe(true);
    expect(states[0]!.value).toMatchObject({
      models: [],
      pending: true,
    });
    snapshot = {
      runtime: {
        modelsByAccount: {
          default: [
            {
              accountKey: "default",
              model: "ready-model",
              provider: "codex",
              hidden: false,
              catalogPosition: 0,
              serviceTiers: [],
              supportedReasoningEfforts: [],
            },
          ],
        },
      },
      entitiesComplete: true,
    };
    acceptSnapshot(snapshot);
    const loadedView = mount(() => useWorkerModels("default", true));
    expect(loadedView.loading).toBe(false);
    expect(states[0]!.value).toMatchObject({
      models: [{ model: "ready-model" }],
      pending: false,
    });
    expect(get).not.toHaveBeenCalled();
  });

  it("applies both cost display projections without GETs", async () => {
    const session = {
      rootId: "lead",
      pricingState: "ready",
      totalUSD: 0.5,
      unknownModels: [],
      breakdown: {},
      estimated: true,
    };
    const account = { accountKey: "default", totalUSD: 1 };
    mount(() =>
      Usage({
        agent: { id: "lead", rootId: "lead", accountKey: "default" } as Agent,
        stateDir: "private-fixture",
        limits: null,
        reload: vi.fn(),
      }),
    );
    expect(subscription).toHaveBeenCalledWith({
      kind: "session-cost",
      agentId: "lead",
    });
    expect(subscription).not.toHaveBeenCalledWith({ kind: "costs" });
    for (const accept of projectionListeners)
      accept({
        runtime: {
          sessionCostsByAgent: { lead: session },
          accountCostsByAccount: { default: account },
        },
      });
    expect(
      states.some(
        (entry) =>
          (entry.value as { value?: unknown } | null)?.value === session,
      ),
    ).toBe(false);
    expect(
      states.some(
        (entry) =>
          (entry.value as { value?: { totalUSD?: number } } | null)?.value
            ?.totalUSD === 0.5,
      ),
    ).toBe(true);
    expect(states.some((entry) => entry.value === account)).toBe(true);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(get).not.toHaveBeenCalled();
    expect(syncPost).not.toHaveBeenCalled();
  });
});
