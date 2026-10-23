import { afterEach, describe, expect, it, vi } from "vitest";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_MISMATCH_FIELD,
} from "../generated/apiSchema";

const workspaceId = "b".repeat(32);
const { resumeListeners, syncDatabase } = vi.hoisted(() => ({
  resumeListeners: new Set<() => void>(),
  syncDatabase: vi.fn(),
}));
vi.mock("./client", () => ({ syncDatabase }));
vi.mock("../usage/tokenRate", async (original) => ({
  ...(await original<typeof import("../usage/tokenRate")>()),
  configureTokenRateStream: vi.fn(),
}));
vi.mock("./resume", () => ({
  onResume: (listener: () => void) => {
    resumeListeners.add(listener);
    return () => resumeListeners.delete(listener);
  },
}));

class Source {
  static instances: Source[] = [];
  static skipNextAutoHandshake = false;
  listeners = new Map<string, Set<(event: MessageEvent<string>) => void>>();
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  status = 200;
  closed = false;
  schemaEmitted = false;
  autoHandshake: boolean;
  constructor(readonly url: string) {
    Source.instances.push(this);
    this.autoHandshake = !Source.skipNextAutoHandshake;
    Source.skipNextAutoHandshake = false;
  }
  addEventListener(
    name: string,
    listener: (event: MessageEvent<string>) => void,
  ) {
    if (name === "session" && this.autoHandshake) {
      listener({
        data: JSON.stringify({ sessionId: "session-test" }),
      } as MessageEvent<string>);
      return;
    }
    if (name === "api-schema" && this.autoHandshake) {
      this.schemaEmitted = true;
      listener({
        data: JSON.stringify({ hash: API_SCHEMA_HASH }),
      } as MessageEvent<string>);
      return;
    }
    let callbacks = this.listeners.get(name);
    if (!callbacks) this.listeners.set(name, (callbacks = new Set()));
    callbacks.add(listener);
  }
  emit(name: string, value: unknown) {
    if (name === "api-schema") this.schemaEmitted = true;
    if (
      name === "resources" &&
      value &&
      typeof value === "object" &&
      !("resourceVersions" in value) &&
      Array.isArray((value as { resources?: unknown }).resources)
    ) {
      const frame = value as {
        revision: number;
        resources: Array<{ kind: string }>;
      };
      value = {
        ...frame,
        resourceVersions: frame.resources.map((resource) => ({
          resource,
          revision: frame.revision,
        })),
      };
    }
    this.emitRaw(name, value);
  }
  emitRaw(name: string, value: unknown) {
    const event = { data: JSON.stringify(value) } as MessageEvent<string>;
    for (const listener of this.listeners.get(name) || []) listener(event);
  }
  close() {
    this.closed = true;
  }
  open() {
    this.onopen?.();
  }
}

class Channel {
  static instances: Channel[] = [];
  onmessage: ((event: MessageEvent) => void) | null = null;
  messages: unknown[] = [];
  constructor(readonly name: string) {
    Channel.instances.push(this);
  }
  postMessage(value: unknown) {
    this.messages.push(value);
  }
  close() {}
}

const activeStops = new Set<() => void>();
const subscriptionPosts: Array<{
  gen: number;
  add: unknown[];
  remove: unknown[];
  tokenRates?: boolean;
}> = [];

function trackStop(stop: () => void) {
  const trackedStop = () => {
    activeStops.delete(trackedStop);
    stop();
  };
  activeStops.add(trackedStop);
  return trackedStop;
}

async function loadTransport() {
  const api = await import("../api");
  vi.spyOn(api, "post").mockImplementation(async (_path, body) => {
    const update = body as {
      gen: number;
      add: unknown[];
      remove: unknown[];
      tokenRates?: boolean;
    };
    subscriptionPosts.push(update);
    queueMicrotask(() =>
      Source.instances.at(-1)?.emit("subscriptions", { gen: update.gen }),
    );
    return { gen: update.gen } as never;
  });
  const transport = await import("./resourceEvents");
  return {
    ...transport,
    watchResourceChanges: (
      ...args: Parameters<typeof transport.watchResourceChanges>
    ) => trackStop(transport.watchResourceChanges(...args)),
    watchResourceConnection: (
      ...args: Parameters<typeof transport.watchResourceConnection>
    ) => trackStop(transport.watchResourceConnection(...args)),
    watchTokenRateEvents: (
      ...args: Parameters<typeof transport.watchTokenRateEvents>
    ) => trackStop(transport.watchTokenRateEvents(...args)),
  };
}

describe("shared resource event transport", () => {
  it("delivers consecutive draft batches without hint coalescing and retains reconnect recovery", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "draft-tab" });
    vi.stubGlobal("EventSource", Source);
    const transport = await loadTransport();
    const listener = vi.fn();
    const resource = { kind: "drafts" } as const;
    const stop = transport.watchResourceChanges(resource, listener);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const emit = (
      revision: number,
      reason: "initial" | "change" | "reconnect",
    ) =>
      Source.instances[0]!.emit("resources", {
        protocol: 4,
        workspaceId,
        epoch: "drafts",
        revision,
        reason,
        resources: [resource],
        resourceVersions: [
          {
            revision,
            ...(reason === "change"
              ? {
                  draftChanges: {
                    after: revision - 1,
                    through: revision,
                    documents: [
                      {
                        id: "phone:chat",
                        seq: revision,
                        payload: "{}",
                        _deleted: false,
                      },
                    ],
                  },
                }
              : {}),
          },
        ],
      });
    emit(0, "initial");
    await vi.waitFor(() => expect(listener).toHaveBeenCalledOnce());
    listener.mockClear();
    emit(1, "change");
    emit(2, "change");
    expect(
      listener.mock.calls.map(([version]) => version.draftChanges.through),
    ).toEqual([1, 2]);
    Source.instances[0]!.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "drafts",
      revision: 3,
      reason: "reconnect",
      resources: [resource],
      resourceVersions: [{ revision: 2 }],
    });
    await vi.waitFor(() => expect(listener).toHaveBeenCalledTimes(3));
    expect(listener.mock.calls[2][0].reason).toBe("reconnect");
    stop();
  });

  afterEach(async () => {
    for (const stop of activeStops) stop();
    const api = await import("../api");
    api.markApiSchemaMismatch();
    vi.useFakeTimers();
    expect(vi.getTimerCount()).toBe(0);
    vi.resetModules();
    vi.restoreAllMocks();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    Source.instances = [];
    subscriptionPosts.length = 0;
    Source.skipNextAutoHandshake = false;
    Channel.instances = [];
    syncDatabase.mockClear();
    resumeListeners.clear();
  });

  it("delivers a hint-only transcript failure even when its publication revision is below the durable cursor", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    const transport = await loadTransport();
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(
      { kind: "transcript", agentId: "open" },
      onChange,
    );
    await vi.advanceTimersByTimeAsync(0);
    const source = Source.instances[0]!;
    const event = {
      protocol: 4,
      workspaceId,
      epoch: "one",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "transcript", agentId: "open" }],
      resourceVersions: [{ revision: 100 }],
    };
    source.emit("resources", event);
    await vi.advanceTimersByTimeAsync(20);
    onChange.mockClear();
    const failure = {
      ...event,
      revision: 2,
      reason: "change",
      resourceVersions: [{ revision: 2 }],
    };
    source.emit("resources", failure);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledOnce();
    source.emit("resources", failure);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledOnce();
    stop();
  });

  it("retries a transient initial identity failure without a page resume", async () => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const stop = transport.watchResourceChanges(
      { kind: "queue", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(0);
    expect(syncDatabase).toHaveBeenCalledTimes(1);
    expect(Source.instances).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(500);
    expect(syncDatabase).toHaveBeenCalledTimes(2);
    expect(Source.instances).toHaveLength(1);
    stop();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(syncDatabase).toHaveBeenCalledTimes(2);
    expect(Source.instances[0]!.closed).toBe(true);
  });

  it("coalesces same-turn subscriptions and never reopens an identical URL", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const stopState = transport.watchResourceChanges(
      { kind: "state" },
      vi.fn(),
    );
    const stopCosts = transport.watchResourceChanges(
      { kind: "costs" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const initialUrl = Source.instances[0]!.url;
    expect(
      JSON.parse(
        new URL(initialUrl, "http://studio.test").searchParams.get(
          "resources",
        )!,
      ),
    ).toHaveLength(2);

    const stopAccounts = transport.watchResourceChanges(
      { kind: "accounts" },
      vi.fn(),
    );
    const stopAccountsAgain = transport.watchResourceChanges(
      { kind: "accounts" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(subscriptionPosts).toHaveLength(1));
    expect(subscriptionPosts[0]?.add).toEqual([{ kind: "accounts" }]);
    expect(Source.instances.filter((source) => !source.closed)).toHaveLength(1);
    const latestUrl = Source.instances[0]!.url;
    expect(latestUrl).toBe(initialUrl);

    await Promise.resolve();
    await Promise.resolve();
    expect(Source.instances.at(-1)!.url).toBe(latestUrl);
    expect(Source.instances).toHaveLength(1);
    stopAccountsAgain();
    stopAccounts();
    stopCosts();
    stopState();
  });

  it("waits for an initial frame and redelivers an unchanged reopen baseline to opted-in readers", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await import("./resourceEvents");
    const resource = { kind: "models" } as const;
    const firstListener = vi.fn();
    const stopFirst = transport.watchResourceChanges(resource, firstListener);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const stream = Source.instances[0]!;
    const initial = {
      protocol: 4,
      workspaceId,
      epoch: "catalog-epoch",
      revision: 3,
      reason: "initial",
      resources: [resource],
      resourceVersions: [{ resource, revision: 3 }],
    };
    stream.emit("resources", initial);
    await vi.waitFor(() => expect(firstListener).toHaveBeenCalledTimes(1));

    const baselineListener = vi.fn();
    const stopBaseline = transport.watchResourceChanges(
      resource,
      baselineListener,
      { waitForBaseline: true },
    );
    expect(baselineListener).toHaveBeenCalledTimes(1);
    expect(baselineListener).toHaveBeenCalledWith({
      epoch: "catalog-epoch",
      revision: 3,
    });
    stream.emit("resources", initial);
    await vi.waitFor(() => expect(baselineListener).toHaveBeenCalledTimes(2));
    expect(baselineListener).toHaveBeenCalledWith({
      epoch: "catalog-epoch",
      revision: 3,
    });
    stopBaseline();
    stopFirst();
  });

  it("suppresses only the exact entity sequence batch applied from a response", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const state = { kind: "state" } as const;
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(state, onChange);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const stream = Source.instances[0]!;
    const emit = (
      revision: number,
      entitySequences?: number[],
      resourceRevision = revision,
    ) =>
      stream.emit("resources", {
        protocol: 4,
        workspaceId,
        epoch: "epoch-one",
        revision,
        reason: "change",
        resources: [state],
        ...(entitySequences
          ? {
              resourceVersions: [
                {
                  revision: resourceRevision,
                  entitySequences,
                },
              ],
            }
          : {}),
      });

    emit(1, []);
    await vi.waitFor(() => expect(onChange).toHaveBeenCalledTimes(1));
    expect(onChange).toHaveBeenLastCalledWith({
      epoch: "epoch-one",
      revision: 1,
      entitySequence: 1,
    });
    transport.acknowledgeEntitySequences([2, 3], workspaceId);
    emit(3, [2, 3]);
    await Promise.resolve();
    expect(onChange).toHaveBeenCalledTimes(1);

    emit(4, [2, 4]);
    await vi.waitFor(() => expect(onChange).toHaveBeenCalledTimes(2));
    expect(onChange).toHaveBeenLastCalledWith({
      epoch: "epoch-one",
      revision: 4,
      entitySequence: 4,
    });
    stream.emitRaw("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 5,
      reason: "change",
      resources: [state],
    });
    await vi.waitFor(() => expect(onChange).toHaveBeenCalledTimes(3));

    transport.acknowledgeEntitySequences([5, 6], workspaceId);
    emit(6, [5, 6]);
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(onChange).toHaveBeenCalledTimes(3);
    emit(7, [5, 6], 6);
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(onChange).toHaveBeenCalledTimes(3);
    stop();
  });

  it("dispatches a reset frame even when the state revision is unchanged", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const state = { kind: "state" } as const;
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(state, onChange);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const stream = Source.instances[0]!;
    stream.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 8,
      reason: "change",
      resources: [state],
      resourceVersions: [{ revision: 8 }],
    });
    await vi.waitFor(() => expect(onChange).toHaveBeenCalledTimes(1));

    stream.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 8,
      reason: "change",
      resources: [state],
      resourceVersions: [{ revision: 8, entitySequenceReset: true }],
    });

    await vi.waitFor(() => expect(onChange).toHaveBeenCalledTimes(2));
    expect(onChange).toHaveBeenLastCalledWith({
      epoch: "epoch-one",
      revision: 8,
      entitySequenceReset: true,
    });
    stop();
  });

  it("rechecks peer subscriptions before the coordinator idle stop", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const ownerChannel = Channel.instances[0]!;
    stop();
    await vi.advanceTimersByTimeAsync(900);
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-peer",
        apiSchemaHash: API_SCHEMA_HASH,
        resources: [{ kind: "costs" }],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);
    await vi.advanceTimersByTimeAsync(100);
    expect(Source.instances).toHaveLength(2);
    expect(Source.instances[1]!.closed).toBe(false);
    expect(Source.instances[1]!.url).not.toBe(Source.instances[0]!.url);
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-peer",
        apiSchemaHash: API_SCHEMA_HASH,
        resources: [],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);
    await vi.advanceTimersByTimeAsync(1_000);
    vi.useRealTimers();
  });

  it("cancels the pending idle-stop timer when the coordinator stops", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    const windowStub = {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    };
    vi.stubGlobal("window", windowStub);
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    stop();
    expect(vi.getTimerCount()).toBeGreaterThan(0);

    const api = await import("../api");
    api.markApiSchemaMismatch();
    expect(vi.getTimerCount()).toBe(0);
    const removeCalls = windowStub.removeEventListener.mock.calls.length;
    await vi.advanceTimersByTimeAsync(1_001);
    expect(windowStub.removeEventListener).toHaveBeenCalledTimes(removeCalls);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("ignores resource-channel messages sent by another schema version", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: null) => Promise<void>,
        ) => Promise.resolve().then(() => callback(null)),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-local" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);
    const transport = await loadTransport();
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(
      { kind: "queue", agentId: "agent-one" },
      onChange,
    );
    await Promise.resolve();
    await Promise.resolve();
    Channel.instances[0]!.onmessage?.({
      data: {
        kind: "resource-event",
        workspaceId,
        tabId: "tab-foreign",
        apiSchemaHash: "foreign-schema",
        event: {
          protocol: 4,
          workspaceId,
          epoch: "epoch-one",
          revision: 1,
          reason: "initial",
          resources: [{ kind: "queue", agentId: "agent-one" }],
        },
      },
    } as MessageEvent);
    Channel.instances[0]!.onmessage?.({
      data: {
        kind: "resource-event",
        workspaceId,
        tabId: "tab-hashless",
        event: {
          protocol: 4,
          workspaceId,
          epoch: "epoch-one",
          revision: 1,
          reason: "initial",
          resources: [{ kind: "queue", agentId: "agent-one" }],
        },
      },
    } as MessageEvent);
    expect(onChange).not.toHaveBeenCalled();
    stop();
  });

  it("stops the live resource stream when its first schema event mismatches", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const stop = transport.watchResourceChanges(
      { kind: "queue", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    source.emit("api-schema", { hash: "foreign-schema" });
    expect(source.closed).toBe(true);
    stop();
  });

  it("treats a mismatch flag as a schema mismatch and closes without reconnecting", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    source.emit("api-schema", {
      hash: API_SCHEMA_HASH,
      [API_SCHEMA_MISMATCH_FIELD]: true,
    });
    expect(source.closed).toBe(true);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(Source.instances).toHaveLength(1);
    stop();
    stopStatus();
  });

  it("fails closed when a named event arrives before the handshake", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const onChange = vi.fn();
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, onChange);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    source.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "state" }],
    });
    expect(source.closed).toBe(true);
    expect(states).toContain("schema-mismatch");
    expect(onChange).not.toHaveBeenCalled();
    stop();
    stopStatus();
  });

  it("retries a pre-open EventSource error without declaring a schema mismatch", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockRejectedValue(new TypeError("Failed to fetch")),
    );
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    source.status = 426;
    source.onerror?.();
    expect(source.closed).toBe(true);
    expect(states).not.toContain("schema-mismatch");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(Source.instances.length).toBeGreaterThan(1);
    stop();
    stopStatus();
  });

  it.each(["rollback", "unavailable"])(
    "checks the identity after a warm stream rejection (%s)",
    async (mode) => {
      vi.useFakeTimers();
      vi.spyOn(Math, "random").mockReturnValue(0.5);
      syncDatabase.mockResolvedValue({ workspaceId });
      vi.stubGlobal("window", {
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
        dispatchEvent: vi.fn(),
      });
      vi.stubGlobal("document", {
        hidden: false,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      });
      vi.stubGlobal("navigator", { onLine: true });
      vi.stubGlobal("location", { origin: "http://studio.test" });
      vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
      vi.stubGlobal("EventSource", Source);
      const fetch = vi.fn(
        async (_request: Request) =>
          new Response(JSON.stringify({ workspaceId }), {
            status: mode === "rollback" ? 200 : 503,
            headers: { "Content-Type": "application/json" },
          }),
      );
      vi.stubGlobal("fetch", fetch);
      const api = await import("../api");
      const transport = await loadTransport();
      const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
      await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
      const live = Source.instances[0]!;
      live.open();
      live.onerror?.();
      expect(fetch).not.toHaveBeenCalled();
      Source.skipNextAutoHandshake = true;
      await vi.advanceTimersByTimeAsync(500);
      const rejected = Source.instances.at(-1)!;
      rejected.onerror?.();
      await vi.advanceTimersByTimeAsync(0);
      expect(fetch).toHaveBeenCalledTimes(1);
      expect(new URL(fetch.mock.calls[0]![0].url).pathname).toBe(
        "/api/sync/identity",
      );
      expect(api.isApiSchemaMismatch()).toBe(mode === "rollback");
      await vi.advanceTimersByTimeAsync(10_000);
      expect(Source.instances.length).toBe(mode === "rollback" ? 2 : 3);
      stop();
    },
  );

  it.each(["unsubscribe", "handshake"])(
    "cancels a shared identity probe on %s",
    async (reason) => {
      vi.useFakeTimers();
      vi.spyOn(Math, "random").mockReturnValue(0.5);
      syncDatabase.mockResolvedValue({ workspaceId });
      vi.stubGlobal("window", {
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
        dispatchEvent: vi.fn(),
      });
      vi.stubGlobal("document", {
        hidden: false,
        addEventListener: vi.fn(),
        removeEventListener: vi.fn(),
      });
      vi.stubGlobal("navigator", { onLine: true });
      vi.stubGlobal("location", { origin: "http://studio.test" });
      vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
      vi.stubGlobal("EventSource", Source);
      const fetch = vi.fn(
        (request: Request) =>
          new Promise<Response>((_resolve, reject) => {
            request.signal.addEventListener(
              "abort",
              () => reject(new DOMException("Aborted", "AbortError")),
              { once: true },
            );
          }),
      );
      vi.stubGlobal("fetch", fetch);
      Source.skipNextAutoHandshake = true;
      const api = await import("../api");
      const transport = await loadTransport();
      const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
      await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
      Source.instances[0]!.onerror?.();
      expect(fetch).toHaveBeenCalledTimes(1);
      const signal = fetch.mock.calls[0]![0].signal;
      Source.skipNextAutoHandshake = true;
      await vi.advanceTimersByTimeAsync(500);
      Source.instances.at(-1)!.onerror?.();
      expect(fetch).toHaveBeenCalledTimes(1);
      expect(signal.aborted).toBe(false);
      if (reason === "unsubscribe") stop();
      else await vi.advanceTimersByTimeAsync(1_000);
      expect(signal.aborted).toBe(true);
      await vi.advanceTimersByTimeAsync(0);
      expect(api.isApiSchemaMismatch()).toBe(false);
      stop();
      await vi.advanceTimersByTimeAsync(10_000);
      expect(fetch).toHaveBeenCalledTimes(1);
    },
  );

  it("restarts the identity probe when a listener returns during idle grace", async () => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    const fetch = vi.fn(
      (request: Request) =>
        new Promise<Response>((_resolve, reject) => {
          request.signal.addEventListener(
            "abort",
            () => reject(new DOMException("Aborted", "AbortError")),
            { once: true },
          );
        }),
    );
    vi.stubGlobal("fetch", fetch);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const stopFirst = transport.watchResourceChanges(
      { kind: "state" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    Source.instances[0]!.onerror?.();
    expect(fetch).toHaveBeenCalledTimes(1);
    const firstSignal = fetch.mock.calls[0]![0].signal;

    stopFirst();
    expect(firstSignal.aborted).toBe(true);
    Source.skipNextAutoHandshake = true;
    const stopSecond = transport.watchResourceChanges(
      { kind: "state" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(500);
    expect(Source.instances).toHaveLength(2);
    Source.instances[1]!.onerror?.();
    expect(fetch).toHaveBeenCalledTimes(2);
    const secondSignal = fetch.mock.calls[1]![0].signal;
    expect(secondSignal.aborted).toBe(false);

    stopSecond();
    expect(secondSignal.aborted).toBe(true);
  });

  it("retries pre-handshake failures three times and ignores a late handshake", async () => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const first = Source.instances[0]!;
    first.open();
    await vi.advanceTimersByTimeAsync(15_000);
    first.emit("api-schema", { hash: API_SCHEMA_HASH });
    expect(first.closed).toBe(true);
    expect(states).not.toContain("schema-mismatch");
    for (let attempt = 2; attempt <= 3; attempt++) {
      Source.skipNextAutoHandshake = true;
      resumeListeners.forEach((resume) => resume());
      await vi.waitFor(() => expect(Source.instances).toHaveLength(attempt));
      const next = Source.instances.at(-1)!;
      next.open();
      next.onerror?.();
      if (attempt < 3) expect(states).not.toContain("schema-mismatch");
    }
    expect(states).toContain("schema-mismatch");
    expect(Source.instances.at(-1)!.closed).toBe(true);
    stop();
    stopStatus();
  });

  it("keeps reconnecting after three silent opens when matching API headers were seen", async () => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal(
      "fetch",
      async () =>
        new Response(JSON.stringify({ token: "session-token" }), {
          headers: {
            "Content-Type": "application/json",
            "X-Studio-API-Schema": API_SCHEMA_HASH,
          },
        }),
    );
    Source.skipNextAutoHandshake = true;
    const api = await import("../api");
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    for (let strike = 0; strike < 3; strike++) {
      const current = Source.instances.at(-1)!;
      current.open();
      await api.get("/api/session");
      current.onerror?.();
      expect(api.isApiSchemaMismatch()).toBe(false);
      expect(states).not.toContain("schema-mismatch");
      await vi.advanceTimersByTimeAsync(10_000);
      expect(Source.instances.length).toBeGreaterThan(strike);
    }
    expect(states).toContain("degraded");
    expect(states).not.toContain("schema-mismatch");
    stop();
    stopStatus();
  });

  it("resets the consecutive pre-handshake failure count on a matching handshake", async () => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const failedOnce = Source.instances[0]!;
    failedOnce.open();
    await vi.advanceTimersByTimeAsync(15_000);

    Source.skipNextAutoHandshake = true;
    await vi.advanceTimersByTimeAsync(1_000);
    const failedTwice = Source.instances.at(-1)!;
    failedTwice.open();
    failedTwice.onerror?.();

    await vi.advanceTimersByTimeAsync(1_500);
    const recovered = Source.instances.at(-1)!;
    recovered.open();
    recovered.emit("api-schema", { hash: API_SCHEMA_HASH });
    recovered.onerror?.();

    for (const retryDelay of [3_000, 5_000]) {
      Source.skipNextAutoHandshake = true;
      const previousCount = Source.instances.length;
      await vi.advanceTimersByTimeAsync(retryDelay);
      expect(Source.instances).toHaveLength(previousCount + 1);
      const next = Source.instances.at(-1)!;
      next.open();
      next.onerror?.();
      expect(states).not.toContain("schema-mismatch");
    }
    Source.skipNextAutoHandshake = true;
    const previousCount = Source.instances.length;
    await vi.advanceTimersByTimeAsync(9_000);
    expect(Source.instances).toHaveLength(previousCount + 1);
    const thirdFailure = Source.instances.at(-1)!;
    thirdFailure.open();
    thirdFailure.onerror?.();
    expect(states).toContain("schema-mismatch");
    stop();
    stopStatus();
  });

  it("reconnects normally when the stream ends after a matching handshake", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    source.open();
    source.onerror?.();
    expect(states).not.toContain("schema-mismatch");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(Source.instances.length).toBeGreaterThan(1);
    stop();
    stopStatus();
  });

  it("fails closed when a frame arrives before the matching stream handshake", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges({ kind: "state" }, onChange);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    source.emitRaw("resources", {
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "state" }],
    });
    expect(onChange).not.toHaveBeenCalled();
    expect(source.closed).toBe(true);
    stop();
  });

  it("degrades and reconnects on malformed same-schema frames without throwing", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    const transport = await loadTransport();
    const statuses: string[] = [];
    const streamStates: string[] = [];
    transport.watchResourceConnection((status, stream) => {
      statuses.push(status);
      streamStates.push(stream);
    });
    expect(streamStates[0]).toBe("stopped");
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    expect(streamStates.at(-1)).toBe("connecting");
    source.open();
    expect(streamStates.at(-1)).toBe("open");
    source.emit("api-schema", { hash: API_SCHEMA_HASH });
    expect(() => source.emitRaw("resources", null)).not.toThrow();
    expect(source.closed).toBe(true);
    expect(statuses).toContain("degraded");
    stop();
  });

  it("degrades and reconnects when a resource subscriber throws", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    const transport = await loadTransport();
    const statuses: string[] = [];
    transport.watchResourceConnection((status) => statuses.push(status));
    const stop = transport.watchResourceChanges({ kind: "state" }, () => {
      throw new Error("subscriber failed");
    });
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    source.emit("api-schema", { hash: API_SCHEMA_HASH });
    source.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "state" }],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(source.closed).toBe(true);
    expect(statuses).toContain("degraded");
    expect(log).toHaveBeenCalledTimes(1);
    stop();
  });

  it("degrades and reconnects on a same-schema peer frame without an event", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    const transport = await loadTransport();
    const statuses: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      statuses.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    Channel.instances[0]!.onmessage?.({
      data: {
        kind: "resource-event",
        workspaceId,
        tabId: "tab-peer",
        apiSchemaHash: API_SCHEMA_HASH,
      },
    } as MessageEvent);
    expect(Source.instances[0]!.closed).toBe(true);
    expect(statuses).toContain("degraded");
    expect(log).toHaveBeenCalledTimes(1);
    stop();
    stopStatus();
  });

  it.each([
    ["null resource data", "resources", null],
    [
      "missing resources",
      "resources",
      { workspaceId, epoch: "epoch-one", revision: 1 },
    ],
    [
      "resources as a string",
      "resources",
      { workspaceId, epoch: "epoch-one", revision: 1, resources: "state" },
    ],
    [
      "non-numeric heartbeat revision",
      "heartbeat",
      { workspaceId, epoch: "epoch-one", revision: "zzz" },
    ],
    [
      "resource frame without revision",
      "resources",
      { workspaceId, epoch: "epoch-one", resources: [] },
    ],
  ])("degrades and reconnects on %s", async (_label, eventName, frame) => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const source = Source.instances[0]!;
    expect(() => source.emitRaw(eventName!, frame)).not.toThrow();
    expect(source.closed).toBe(true);
    expect(states).toContain("degraded");
    stop();
    stopStatus();
  });

  it("degrades and reconnects when a peer message has no event payload", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true, locks: undefined });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    Channel.instances[0]!.onmessage?.({
      data: {
        kind: "resource-event",
        workspaceId,
        tabId: "peer-tab",
        apiSchemaHash: API_SCHEMA_HASH,
      },
    } as MessageEvent);
    expect(Source.instances[0]!.closed).toBe(true);
    expect(states).toContain("degraded");
    stop();
    stopStatus();
  });

  it("notifies only the matching resource once per revision and preserves liveness on reconfigure", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const queue = { kind: "queue", agentId: "agent-one" } as const;
    const terminal = { kind: "terminal", terminalId: "terminal-one" } as const;
    const queueChanges = vi.fn();
    const terminalChanges = vi.fn();
    const states: string[] = [];
    const stopQueue = transport.watchResourceChanges(queue, queueChanges);
    const stopStatus = transport.watchResourceConnection((state) =>
      states.push(state),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const first = Source.instances[0]!;
    first.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [queue],
      resourceVersions: [{ resource: queue, revision: 1 }],
    });
    await vi.waitFor(() => expect(queueChanges).toHaveBeenCalledTimes(1));
    first.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "change",
      resources: [queue],
      resourceVersions: [{ resource: queue, revision: 1 }],
    });
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(queueChanges).toHaveBeenCalledTimes(1);
    first.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 2,
      reason: "change",
      resources: [{ kind: "queue", agentId: "another-agent" }],
      resourceVersions: [
        { resource: { kind: "queue", agentId: "another-agent" }, revision: 2 },
      ],
    });
    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(queueChanges).toHaveBeenCalledTimes(1);

    const stopTerminal = transport.watchResourceChanges(
      terminal,
      terminalChanges,
    );
    await vi.waitFor(() => expect(subscriptionPosts).toHaveLength(1));
    expect(Source.instances).toHaveLength(1);
    const replacement = Source.instances[0]!;
    expect(states.slice(2).every((state) => state === "live")).toBe(true);
    expect(states.at(-1)).toBe("live");
    replacement.emit("api-schema", { hash: API_SCHEMA_HASH });
    replacement.onerror?.();
    expect(states.at(-1)).toBe("degraded");
    expect(terminalChanges).not.toHaveBeenCalled();

    stopTerminal();
    stopQueue();
    stopStatus();
    expect(Source.instances.every((source) => source.closed)).toBe(true);
    expect(syncDatabase).toHaveBeenCalledTimes(1);
  });

  it("keeps a quiet stream live across repeated server heartbeat intervals", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges(
      { kind: "queue", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const stream = Source.instances[0]!;

    for (let revision = 1; revision <= 4; revision++) {
      stream.emit("heartbeat", {
        protocol: 4,
        workspaceId,
        epoch: "epoch-one",
        revision,
      });
      await vi.advanceTimersByTimeAsync(15_000);
    }

    expect(Source.instances).toHaveLength(1);
    expect(stream.closed).toBe(false);
    expect(syncDatabase).toHaveBeenCalledTimes(1);
    stop();
    vi.useRealTimers();
  });

  it("takes a queued Web Lock as soon as the current owner releases it", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    let releaseQueuedRequest: (() => void) | undefined;
    const request = vi.fn(
      (
        _name: string,
        options: { signal: AbortSignal },
        callback: (lock: {}) => Promise<void>,
      ) =>
        new Promise<void>((resolve, reject) => {
          const abort = () => reject(new DOMException("Aborted", "AbortError"));
          options.signal.addEventListener("abort", abort, { once: true });
          releaseQueuedRequest = () => {
            options.signal.removeEventListener("abort", abort);
            void callback({}).then(resolve, reject);
          };
        }),
    );
    vi.stubGlobal("navigator", { onLine: true, locks: { request } });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-waiter" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    const onTokenRates = vi.fn();
    const stopTokens = transport.watchTokenRateEvents(onTokenRates);
    await vi.waitFor(() => expect(request).toHaveBeenCalledTimes(1));
    expect(request.mock.calls[0]?.[1].signal.aborted).toBe(false);
    expect(Source.instances).toHaveLength(0);

    // The mock delivers the held native lock only after its prior owner releases.
    releaseQueuedRequest?.();
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    expect(Source.instances[0]?.closed).toBe(false);
    expect(
      new URL(Source.instances[0]!.url, "http://studio.test").searchParams.get(
        "tokenRates",
      ),
    ).toBe("true");
    const currentSnapshot = {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      snapshot: true,
      removed: [],
      records: [],
    };
    Source.instances[0]!.emit("token-rates", currentSnapshot);
    expect(onTokenRates).toHaveBeenCalledWith(currentSnapshot);
    stopTokens();
    await vi.waitFor(() => expect(subscriptionPosts).toHaveLength(1));
    expect(subscriptionPosts[0]?.tokenRates).toBe(false);
    expect(Source.instances).toHaveLength(1);
    stop();
    await vi.waitFor(() => expect(Source.instances[0]?.closed).toBe(true));
  });

  it("cancels a queued Web Lock on hide without falling back to a second stream", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const documentListeners = new Map<string, () => void>();
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn((name: string, listener: () => void) =>
        documentListeners.set(name, listener),
      ),
      removeEventListener: vi.fn(),
    });
    let requestSignal: AbortSignal | undefined;
    const request = vi.fn(
      (
        _name: string,
        options: { signal: AbortSignal },
        _callback: (lock: {}) => Promise<void>,
      ) =>
        new Promise<void>((_resolve, reject) => {
          requestSignal = options.signal;
          options.signal.addEventListener(
            "abort",
            () => reject(new DOMException("Aborted", "AbortError")),
            { once: true },
          );
        }),
    );
    vi.stubGlobal("navigator", { onLine: true, locks: { request } });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-hidden-waiter" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.waitFor(() => expect(request).toHaveBeenCalledTimes(1));
    expect(Source.instances).toHaveLength(0);

    (document as unknown as { hidden: boolean }).hidden = true;
    documentListeners.get("visibilitychange")?.();
    await vi.waitFor(() => expect(requestSignal?.aborted).toBe(true));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(Source.instances).toHaveLength(0);
    stop();
  });

  it("opens only one replacement stream when online and pageshow resume overlap", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const windowListeners = new Map<string, () => void>();
    const windowStub = {
      addEventListener: vi.fn((name: string, listener: () => void) =>
        windowListeners.set(name, listener),
      ),
      removeEventListener: vi.fn(),
    };
    const navigatorStub = { onLine: true };
    vi.stubGlobal("window", windowStub);
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", navigatorStub);
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges(
      { kind: "voice", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const first = Source.instances[0]!;
    first.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "voice", agentId: "agent-one" }],
    });
    await vi.waitFor(() => expect(first.closed).toBe(false));

    navigatorStub.onLine = false;
    windowListeners.get("offline")?.();
    expect(first.closed).toBe(true);
    navigatorStub.onLine = true;
    windowListeners.get("online")?.();
    for (const listener of resumeListeners) listener();

    await vi.waitFor(() => expect(Source.instances).toHaveLength(2));
    expect(Source.instances.filter((source) => !source.closed)).toHaveLength(1);
    stop();
  });

  it("treats valid busy-stream events as liveness and reconnects after silence", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const resource = { kind: "costs" } as const;
    const onChange = vi.fn();
    const states: string[] = [];
    const stopChanges = transport.watchResourceChanges(resource, onChange);
    const stopStatus = transport.watchResourceConnection((state) =>
      states.push(state),
    );
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const first = Source.instances[0]!;

    for (let revision = 1; revision <= 6; revision++) {
      first.emit("resources", {
        protocol: 4,
        workspaceId,
        epoch: "epoch-one",
        revision,
        reason: revision === 1 ? "initial" : "change",
        resources: [resource],
      });
      await vi.advanceTimersByTimeAsync(10_000);
    }
    expect(Source.instances).toHaveLength(1);
    expect(states).not.toContain("degraded");
    expect(onChange).toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(46_000);
    expect(Source.instances).toHaveLength(2);
    expect(states).toContain("degraded");
    const replacement = Source.instances[1]!;
    replacement.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "reconnect",
      resources: [resource],
    });
    await vi.advanceTimersByTimeAsync(25);
    expect(states.at(-1)).toBe("live");
    stopChanges();
    stopStatus();
    vi.useRealTimers();
  });

  it("sends aggregate token interest through follower changes and owner reacquisition", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    const visibility = {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    };
    vi.stubGlobal("document", visibility);
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);
    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "drafts" }, vi.fn());
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const interest = (source: Source) =>
      new URL(source.url, "http://studio.test").searchParams.get("tokenRates");
    expect(interest(Source.instances[0]!)).toBe("false");
    const channel = Channel.instances[0]!;
    const peerInterest = (tokenRates: boolean) =>
      channel.onmessage?.({
        data: {
          kind: "subscriptions",
          workspaceId,
          apiSchemaHash: API_SCHEMA_HASH,
          tabId: "tab-follower",
          resources: [],
          tokenRates,
          reset: true,
        },
      } as MessageEvent);
    peerInterest(true);
    await vi.waitFor(() => expect(subscriptionPosts).toHaveLength(1));
    expect(Source.instances[0]!.closed).toBe(false);
    expect(subscriptionPosts[0]?.tokenRates).toBe(true);
    const event = {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 3,
      snapshot: true,
      removed: [],
      records: [],
    };
    const beforeSnapshot = channel.messages.length;
    Source.instances[0]!.emit("token-rates", event);
    expect(channel.messages.slice(beforeSnapshot)).toContainEqual(
      expect.objectContaining({ kind: "token-rates", event }),
    );
    peerInterest(false);
    await vi.waitFor(() => expect(subscriptionPosts).toHaveLength(2));
    expect(subscriptionPosts[1]?.tokenRates).toBe(false);
    peerInterest(true);
    await vi.waitFor(() => expect(subscriptionPosts).toHaveLength(3));
    expect(subscriptionPosts[2]?.tokenRates).toBe(true);
    expect(Source.instances).toHaveLength(1);
    visibility.hidden = true;
    for (const resume of resumeListeners) resume();
    expect(Source.instances[0]!.closed).toBe(true);
    visibility.hidden = false;
    for (const resume of resumeListeners) resume();
    await vi.waitFor(() => expect(Source.instances).toHaveLength(2));
    expect(interest(Source.instances[1]!)).toBe("true");
    const beforeTakeover = channel.messages.length;
    Source.instances[1]!.emit("token-rates", { ...event, revision: 4 });
    expect(channel.messages.slice(beforeTakeover)).toContainEqual(
      expect.objectContaining({
        kind: "token-rates",
        event: { ...event, revision: 4 },
      }),
    );
    stop();
  });

  it("delivers resource and token-rate events that share a hub revision", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("BroadcastChannel", Channel);
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const resource = { kind: "costs" } as const;
    const onResourceChange = vi.fn();
    const onTokenRates = vi.fn();
    const stopResource = transport.watchResourceChanges(
      resource,
      onResourceChange,
    );
    const stopTokenRates = transport.watchTokenRateEvents(onTokenRates);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const stream = Source.instances[0]!;

    stream.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 3,
      reason: "change",
      resources: [resource],
    });
    stream.emit("token-rates", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 3,
      snapshot: true,
      removed: [],
      records: [
        {
          agentId: "worker",
          rootId: "root",
          turnId: "turn",
          active: true,
          estimated: false,
          rate: 42,
          outputTokens: 84,
        },
      ],
    });

    await vi.waitFor(() => expect(onResourceChange).toHaveBeenCalledTimes(1));
    expect(onTokenRates).toHaveBeenCalledTimes(1);
    const first = onTokenRates.mock.calls[0]![0];
    const changed = { ...first.records[0], rate: 64, rootId: "other-root" };
    stream.emit("token-rates", {
      ...first,
      revision: 4,
      snapshot: false,
      records: [changed],
    });
    const event = { ...first, revision: 4, records: [changed] };
    const late = vi.fn();
    const stopLate = transport.watchTokenRateEvents(late);
    expect(late).toHaveBeenCalledExactlyOnceWith(event);
    const channel = Channel.instances[0]!;
    channel.messages.length = 0;
    channel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "late-follower",
        apiSchemaHash: API_SCHEMA_HASH,
        resources: [],
        tokenRates: true,
        reset: true,
      },
    } as MessageEvent);
    expect(channel.messages).toContainEqual(
      expect.objectContaining({ kind: "token-rates", event }),
    );
    stopLate();
    stopTokenRates();
    stopResource();
  });

  it("replays a baseline for a late peer ref already present in an earlier SSE event", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const local = { kind: "drafts" } as const;
    const latePeerRef = { kind: "state" } as const;
    const localChanges = vi.fn();
    const stop = transport.watchResourceChanges(local, localChanges);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const ownerChannel = Channel.instances[0]!;
    Source.instances[0]!.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [local, latePeerRef],
      resourceVersions: [
        { resource: local, revision: 7 },
        { resource: latePeerRef, revision: 7, entitySequences: [42] },
      ],
    });
    await new Promise((resolve) => setTimeout(resolve, 30));

    expect(localChanges).toHaveBeenCalledTimes(1);

    const sentBeforeLateSubscription = ownerChannel.messages.length;
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-follower",
        apiSchemaHash: API_SCHEMA_HASH,
        resources: [latePeerRef],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);

    expect(
      ownerChannel.messages
        .slice(sentBeforeLateSubscription)
        .some(
          (message: any) =>
            message.kind === "resource-event" &&
            message.event.resources.some(
              (resource: any) => resource.kind === "state",
            ) &&
            message.event.resourceVersions[0]?.entitySequences?.[0] === 42,
        ),
    ).toBe(true);
    stop();
  });

  it("bounds inactive versions and reconciles an evicted late peer ref", async () => {
    vi.useFakeTimers();
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const local = { kind: "state" } as const;
    const bulkRefs = Array.from({ length: 129 }, (_, index) => ({
      kind: "queue" as const,
      agentId: `inactive-${index}`,
    }));
    const evictedRef = bulkRefs[0]!;
    const localChanges = vi.fn();
    const stop = transport.watchResourceChanges(local, localChanges);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const ownerChannel = Channel.instances[0]!;
    Source.instances[0]!.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [local, ...bulkRefs],
    });
    await vi.advanceTimersByTimeAsync(20);

    const beforeCacheInspection = ownerChannel.messages.length;
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-cache-inspection",
        apiSchemaHash: API_SCHEMA_HASH,
        resources: [local, ...bulkRefs],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);
    const cachedBaseline = ownerChannel.messages
      .slice(beforeCacheInspection)
      .find((message: any) => message.kind === "resource-event") as
      | { event: { resources: unknown[] } }
      | undefined;
    const cachedResources = cachedBaseline?.event.resources ?? [];
    expect(cachedResources).toHaveLength(129);
    expect(cachedResources).toContainEqual(local);
    expect(cachedResources).toContainEqual(bulkRefs.at(-1));
    expect(cachedResources).not.toContainEqual(evictedRef);
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-cache-inspection",
        apiSchemaHash: API_SCHEMA_HASH,
        resources: [],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);

    const sentBeforeLateSubscription = ownerChannel.messages.length;
    const sourceCountBeforeLateSubscription = Source.instances.length;
    ownerChannel.onmessage?.({
      data: {
        kind: "subscriptions",
        workspaceId,
        tabId: "tab-follower",
        apiSchemaHash: API_SCHEMA_HASH,
        resources: [evictedRef],
        tokenRates: false,
        reset: true,
      },
    } as MessageEvent);

    await vi.waitFor(() => expect(subscriptionPosts).toHaveLength(1));
    expect(subscriptionPosts[0]?.add).toEqual([evictedRef]);
    expect(Source.instances).toHaveLength(sourceCountBeforeLateSubscription);
    expect(
      ownerChannel.messages
        .slice(sentBeforeLateSubscription)
        .some((message: any) => message.kind === "resource-event"),
    ).toBe(false);

    Source.instances.at(-1)!.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 8,
      reason: "initial",
      resources: [local, evictedRef],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(
      ownerChannel.messages
        .slice(sentBeforeLateSubscription)
        .some(
          (message: any) =>
            message.kind === "resource-event" &&
            message.event.resources.some(
              (resource: any) =>
                resource.kind === "queue" &&
                resource.agentId === evictedRef.agentId,
            ),
        ),
    ).toBe(true);
    expect(localChanges).toHaveBeenCalledTimes(2);
    stop();
  });

  it("reconciles an unchanged owner baseline once after reconnect", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.useFakeTimers();
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const local = { kind: "queue", agentId: "resume-queue" } as const;
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(local, onChange);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
    const initial = {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [local],
      resourceVersions: [{ resource: local, revision: 7 }],
    };
    Source.instances[0]!.emit("resources", initial);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(1);

    Source.instances[0]!.onerror?.();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(Source.instances).toHaveLength(2);
    Source.instances[1]!.emit("resources", {
      ...initial,
      reason: "reconnect",
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);

    Source.instances[1]!.emit("resources", initial);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);

    Channel.instances[0]!.onmessage?.({
      data: {
        kind: "resource-event",
        workspaceId,
        tabId: "tab-peer",
        apiSchemaHash: API_SCHEMA_HASH,
        event: initial,
      },
    } as MessageEvent);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);
    stop();
  });

  it("recovers from a replaced stream with a new server epoch", async () => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
    vi.stubGlobal("EventSource", Source);

    const transport = await loadTransport();
    const resource = { kind: "state" } as const;
    const onChange = vi.fn();
    const states: string[] = [];
    const stopChanges = transport.watchResourceChanges(resource, onChange);
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    try {
      await vi.waitFor(() => expect(Source.instances).toHaveLength(1));
      const original = Source.instances[0]!;
      original.emit("resources", {
        protocol: 4,
        workspaceId,
        epoch: "server-before-restart",
        revision: 12,
        reason: "initial",
        resources: [resource],
      });
      await vi.advanceTimersByTimeAsync(25);
      expect(states.at(-1)).toBe("live");

      original.onerror?.();
      await vi.advanceTimersByTimeAsync(500);
      expect(Source.instances).toHaveLength(2);
      Source.instances[1]!.emit("resources", {
        protocol: 4,
        workspaceId,
        epoch: "server-after-restart",
        revision: 1,
        reason: "initial",
        resources: [resource],
      });
      await vi.advanceTimersByTimeAsync(25);

      expect(states.at(-1)).toBe("live");
      expect(onChange).toHaveBeenCalledTimes(2);
    } finally {
      stopChanges();
      stopStatus();
      vi.useRealTimers();
    }
  });

  it("reconciles an unchanged follower baseline once and forgets unsubscribed markers", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: null) => Promise<void>,
        ) => Promise.resolve().then(() => callback(null)),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-follower" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const a = { kind: "queue", agentId: "follower-resume-a" } as const;
    const b = { kind: "queue", agentId: "follower-resume-b" } as const;
    const onA = vi.fn();
    const onB = vi.fn();
    let stopA = transport.watchResourceChanges(a, onA);
    const stopB = transport.watchResourceChanges(b, onB);
    await Promise.resolve();
    await Promise.resolve();
    expect(Source.instances).toHaveLength(0);
    const followerChannel = Channel.instances[0]!;
    const baseline = {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [a, b],
      resourceVersions: [
        { resource: a, revision: 7 },
        { resource: b, revision: 7 },
      ],
    };
    const send = (kind: string, value: unknown) =>
      followerChannel.onmessage?.({
        data: {
          kind,
          workspaceId,
          tabId: "tab-owner",
          apiSchemaHash: API_SCHEMA_HASH,
          ...(kind === "resource-event" ? { event: value } : { status: value }),
        },
      } as MessageEvent);

    send("resource-event", baseline);
    await vi.waitFor(() => {
      expect(onA).toHaveBeenCalledTimes(1);
      expect(onB).toHaveBeenCalledTimes(1);
    });
    send("status", "degraded");
    send("resource-event", { ...baseline, reason: "reconnect" });
    await vi.waitFor(() => {
      expect(onA).toHaveBeenCalledTimes(2);
      expect(onB).toHaveBeenCalledTimes(2);
    });

    send("resource-event", { ...baseline, reason: "reconnect" });
    send("leader-heartbeat", undefined);
    send("leader-heartbeat", undefined);
    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(onA).toHaveBeenCalledTimes(2);
    expect(onB).toHaveBeenCalledTimes(2);
    expect(Source.instances).toHaveLength(0);

    send("status", "degraded");
    stopA();
    const resumedA = vi.fn();
    stopA = transport.watchResourceChanges(a, resumedA);
    expect(resumedA).toHaveBeenCalledTimes(1);
    send("resource-event", { ...baseline, reason: "reconnect" });
    await vi.waitFor(() => expect(onB).toHaveBeenCalledTimes(3));
    await new Promise((resolve) => setTimeout(resolve, 40));
    expect(resumedA).toHaveBeenCalledTimes(1);
    expect(Source.instances).toHaveLength(0);

    stopA();
    stopB();
  });

  it("drops a recovery marker when its last local subscriber leaves", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.useFakeTimers();
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", {
      onLine: true,
      locks: {
        request: (
          _name: string,
          _options: unknown,
          callback: (lock: {}) => Promise<void>,
        ) => Promise.resolve().then(() => callback({})),
      },
    });
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("EventSource", Source);
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const a = { kind: "queue", agentId: "resume-queue-a" } as const;
    const b = { kind: "queue", agentId: "resume-queue-b" } as const;
    const onA = vi.fn();
    const onB = vi.fn();
    let stopA = transport.watchResourceChanges(a, onA);
    const stopB = transport.watchResourceChanges(b, onB);
    await vi.waitFor(() => expect(Source.instances).toHaveLength(1));

    const baseline = {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [a, b],
      resourceVersions: [
        { resource: a, revision: 7 },
        { resource: b, revision: 7 },
      ],
    };
    Source.instances[0]!.emit("resources", baseline);
    await vi.advanceTimersByTimeAsync(20);
    expect(onA).toHaveBeenCalledTimes(1);
    expect(onB).toHaveBeenCalledTimes(1);

    Source.instances[0]!.onerror?.();
    stopA();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(Source.instances).toHaveLength(2);
    Source.instances[1]!.emit("resources", {
      ...baseline,
      reason: "reconnect",
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(onB).toHaveBeenCalledTimes(2);

    const resumedA = vi.fn();
    stopA = transport.watchResourceChanges(a, resumedA);
    expect(resumedA).toHaveBeenCalledTimes(1);
    Source.instances[1]!.emit("resources", baseline);
    await vi.advanceTimersByTimeAsync(20);
    expect(resumedA).toHaveBeenCalledTimes(1);
    stopA();
    stopB();
  });
});
