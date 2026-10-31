import assert from "node:assert/strict";
import type { ResourceConnectionDetail } from "./resourceEvents";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_MISMATCH_FIELD,
} from "../generated/apiSchema";

const workspaceId = "b".repeat(32);
const { syncDatabase, appendAppLog } = vi.hoisted(() => ({
  syncDatabase: vi.fn(),
  appendAppLog: vi.fn(),
}));
vi.mock("../appLog", () => ({ appendAppLog }));
vi.mock("../uuid", () => ({ randomUUID: () => crypto.randomUUID() }));
vi.mock("../servers/eventSource", () => ({
  openServerEvents: (url: string) =>
    new (
      (globalThis as { NativeTestSource?: typeof EventSource })
        .NativeTestSource ?? EventSource
    )(url),
}));
vi.mock("./client", () => ({
  syncDatabase,
  readSubscriptionCursor: vi.fn(async () => undefined),
  persistResourceEntityChanges: vi.fn(async () => {}),
}));
vi.mock("../usage/tokenRate", async (original) => ({
  ...(await original<typeof import("../usage/tokenRate")>()),
  configureTokenRateStream: vi.fn(),
}));

class Source {
  readonly url: string;
  static instances: Source[] = [];
  static skipNextAutoHandshake = false;
  listeners = new Map<string, Set<(event: MessageEvent<string>) => void>>();
  onerror: ((event?: { status?: number }) => void) | null = null;
  onopen: (() => void) | null = null;
  status = 200;
  closed = false;
  schemaEmitted = false;
  autoHandshake: boolean;
  constructor(url: string) {
    this.url = url;

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
        data: JSON.stringify({
          sessionId: "session-test",
          workspaceId,
          routes: [],
        }),
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

/** A source whose reconnects belong to the native core (native/stream.ts). */
class NativeSource extends Source {
  restart = vi.fn();
  resume = vi.fn();
  onretry: ((detail: { opened: boolean; retryAt: number }) => void) | null =
    null;
  onresume: (() => void) | null = null;
}

class Channel {
  readonly name: string;
  static instances: Channel[] = [];
  onmessage: ((event: MessageEvent) => void) | null = null;
  messages: unknown[] = [];
  constructor(name: string) {
    this.name = name;

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

async function expectRecovered(listener: ReturnType<typeof vi.fn>) {
  const before = Source.instances.length;
  await vi.advanceTimersByTimeAsync(750);
  expect(Source.instances).toHaveLength(before + 1);
  const replacement = Source.instances.at(-1)!;
  replacement.open();
  replacement.emit("resources", {
    protocol: 4,
    workspaceId,
    epoch: "recovered",
    revision: 2,
    reason: "reconnect",
    resources: [{ kind: "state" }],
  });
  await vi.advanceTimersByTimeAsync(20);
  expect(listener).toHaveBeenLastCalledWith(
    expect.objectContaining({ epoch: "recovered" }),
  );
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

beforeEach(async () => {
  vi.useFakeTimers();
  appendAppLog.mockClear();
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
  const { readSubscriptionCursor } = await import("./client");
  vi.mocked(readSubscriptionCursor).mockReset().mockResolvedValue(undefined);
});

describe("shared resource event transport", () => {
  it("delivers rename failure as transient progress for its exact agent and request", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const transport = await loadTransport();
    const accept = vi.fn();
    const resource = {
      kind: "rename-progress" as const,
      agentId: "agent",
      requestId: "rename-one",
    };
    transport.watchResourceChanges(resource, accept);
    await vi.advanceTimersByTimeAsync(0);
    expect(Source.instances).toHaveLength(1);
    const value = {
      id: "agent",
      request_id: "rename-one",
      status: "failed",
      error: "No title",
    };
    Source.instances[0]!.emit("resource-progress", {
      protocol: 4,
      workspaceId,
      epoch: "rename",
      revision: 1,
      resource,
      value,
    });
    expect(accept).toHaveBeenCalledOnce();
    assert.ok(accept.mock.calls[0] != null);
    expect(accept.mock.calls[0][0].progress).toEqual({ resource, value });
  });

  it("delivers transient discovery and cursor voice payloads without entity persistence", async () => {
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
    vi.stubGlobal("crypto", { randomUUID: () => "transient-tab" });
    vi.stubGlobal("EventSource", Source);
    const transport = await loadTransport();
    const discovery = vi.fn();
    const voice = vi.fn();
    transport.watchResourceChanges({ kind: "multi-server" }, discovery);
    transport.watchResourceChanges({ kind: "voice", agentId: "chat" }, voice);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const snapshot = {
      protocol: 1,
      identity: { serverId: "owner" },
      clients: [],
      servers: [],
      invites: [],
    };
    const batch = {
      after: 0,
      cursor: 1,
      records: [{ id: "record" }],
      session: null,
    };
    Source.instances[0]!.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "transient",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "multi-server" }, { kind: "voice", agentId: "chat" }],
      resourceVersions: [
        { revision: 1, discoverySnapshot: snapshot },
        { revision: 1, voiceRecords: batch },
      ],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(voice).toHaveBeenCalledOnce();
    assert.ok(discovery.mock.calls[0] != null);
    expect(discovery.mock.calls[0][0].discoverySnapshot).toEqual(snapshot);
    assert.ok(voice.mock.calls[0] != null);
    expect(voice.mock.calls[0][0].voiceRecords).toEqual(batch);
  });

  it("delivers consecutive draft batches without hint coalescing and retains reconnect recovery", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("crypto", { randomUUID: () => "draft-tab" });
    const transport = await loadTransport();
    const listener = vi.fn();
    const resource = { kind: "drafts" } as const;
    const stop = transport.watchResourceChanges(resource, listener);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    await vi.advanceTimersByTimeAsync(20);
    expect(listener).toHaveBeenCalledOnce();
    listener.mockClear();
    emit(1, "change");
    emit(2, "change");
    await vi.advanceTimersByTimeAsync(0);
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
    await vi.advanceTimersByTimeAsync(20);
    expect(listener).toHaveBeenCalledTimes(3);
    assert.ok(listener.mock.calls[2] != null);
    expect(listener.mock.calls[2][0].reason).toBe("reconnect");
    stop();
  });

  afterEach(async () => {
    for (const stop of activeStops) stop();
    await vi.advanceTimersByTimeAsync(0);
    const api = await import("../api");
    api.markApiSchemaMismatch();
    // Drain deferred coordinator shutdown without assuming a fixed timer deadline.
    // The assertion still rejects recurring timers after cleanup.
    await vi.runOnlyPendingTimersAsync();
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
  });

  it("delivers a hint-only transcript failure even when its publication revision is below the durable cursor", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
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
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockRejectedValueOnce(new TypeError("Failed to fetch"));
    syncDatabase.mockResolvedValue({ workspaceId });
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

    const transport = await loadTransport();
    const stopState = transport.watchResourceChanges(
      { kind: "state" },
      vi.fn(),
    );
    const stopCosts = transport.watchResourceChanges(
      { kind: "costs" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    await vi.advanceTimersByTimeAsync(30);
    expect(subscriptionPosts).toHaveLength(1);
    expect(subscriptionPosts[0]?.add).toEqual([
      { resource: { kind: "accounts" }, since: undefined },
    ]);
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

    const transport = await loadTransport();
    const resource = { kind: "models" } as const;
    const firstListener = vi.fn();
    const stopFirst = transport.watchResourceChanges(resource, firstListener);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    await vi.advanceTimersByTimeAsync(20);
    expect(firstListener).toHaveBeenCalledTimes(1);

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
    await vi.advanceTimersByTimeAsync(20);
    expect(baselineListener).toHaveBeenCalledTimes(2);
    expect(baselineListener).toHaveBeenCalledWith({
      epoch: "catalog-epoch",
      revision: 3,
    });
    stopBaseline();
    stopFirst();
  });

  it.each(["success", "failure", "pending"] as const)(
    "delivers other writers' stream changes while a mutation is %s",
    async (outcome) => {
      syncDatabase.mockResolvedValue({ workspaceId });
      const api = await import("../api");
      // Keep the real mutation caller; loadTransport mocks subscription updates.
      const post = api.post;
      const transport = await loadTransport();
      api.setToken("test-token");
      api.setWorkspace(workspaceId);
      let resolve!: (response: Response) => void;
      let reject!: (error: Error) => void;
      const fetch = vi.fn(
        () =>
          new Promise<Response>((done, fail) => {
            resolve = done;
            reject = fail;
          }),
      );
      vi.stubGlobal("fetch", fetch);
      const onChange = vi.fn();
      const stop = transport.watchResourceChanges({ kind: "state" }, onChange);
      await vi.advanceTimersByTimeAsync(20);
      const emit = (revision: number, reason = "change") =>
        Source.instances[0]!.emit("resources", {
          protocol: 4,
          workspaceId,
          epoch: "mutation",
          revision,
          reason,
          resources: [{ kind: "state" }],
        });
      emit(40, "initial");
      await vi.advanceTimersByTimeAsync(20);
      expect(onChange).toHaveBeenCalledOnce();
      const pending = post("/api/stop" as never, { id: "chat" } as never).catch(
        (error) => error,
      );
      await vi.advanceTimersByTimeAsync(0);
      expect(fetch).toHaveBeenCalledOnce();
      // Both writers publish before the mutation response arrives.
      emit(41);
      await vi.advanceTimersByTimeAsync(5);
      emit(42);
      if (outcome === "failure") reject(new TypeError("response lost"));
      else if (outcome === "success") resolve(Response.json({ ok: true }));
      if (outcome !== "pending") await pending;
      await vi.advanceTimersByTimeAsync(20);
      expect(onChange.mock.calls.map(([version]) => version.revision)).toEqual([
        40, 41, 42,
      ]);
      expect(onChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ revision: 42 }),
      );
      await vi.advanceTimersByTimeAsync(20_000);
      emit(43);
      await vi.advanceTimersByTimeAsync(20);
      expect(onChange).toHaveBeenCalledTimes(4);
      expect(onChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ revision: 43 }),
      );
      if (outcome === "pending") {
        resolve(Response.json({ ok: true }));
        await pending;
      }
      stop();
    },
  );

  it("suppresses only the exact entity sequence batch applied from a response", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });

    const transport = await loadTransport();
    const state = { kind: "state" } as const;
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(state, onChange);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(1);
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
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);
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
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(3);

    transport.acknowledgeEntitySequences([5, 6], workspaceId);
    emit(6, [5, 6]);
    await vi.advanceTimersByTimeAsync(30);
    expect(onChange).toHaveBeenCalledTimes(3);
    emit(7, [5, 6], 6);
    await vi.advanceTimersByTimeAsync(30);
    expect(onChange).toHaveBeenCalledTimes(3);
    stop();
  });

  it("dispatches a reset frame even when the state revision is unchanged", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });

    const transport = await loadTransport();
    const state = { kind: "state" } as const;
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(state, onChange);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(1);

    stream.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 8,
      reason: "change",
      resources: [state],
      resourceVersions: [{ revision: 8, entitySequenceReset: true }],
    });

    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);
    expect(onChange).toHaveBeenLastCalledWith({
      epoch: "epoch-one",
      revision: 8,
      entitySequenceReset: true,
    });
    stop();
  });

  it("cancels the pending idle-stop timer when the coordinator stops", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const windowStub = {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    };
    vi.stubGlobal("window", windowStub);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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

  it("stops the live resource stream when its first schema event mismatches", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const stop = transport.watchResourceChanges(
      { kind: "queue", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const source = Source.instances[0]!;
    source.emit("api-schema", { hash: "foreign-schema" });
    expect(source.closed).toBe(true);
    stop();
  });

  it("treats a mismatch flag as a schema mismatch and closes without reconnecting", async () => {
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const source = Source.instances[0]!;
    source.emit("api-schema", {
      hash: API_SCHEMA_HASH,
      [API_SCHEMA_MISMATCH_FIELD]: true,
    });
    expect(source.closed).toBe(true);
    const identityRequests = syncDatabase.mock.calls.length;
    expect((await import("../api")).isApiSchemaMismatch()).toBe(true);
    await vi.advanceTimersByTimeAsync(30_000);
    expect(Source.instances).toHaveLength(1);
    expect(syncDatabase).toHaveBeenCalledTimes(identityRequests);
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
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const onChange = vi.fn();
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, onChange);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    syncDatabase.mockResolvedValue({ workspaceId });
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
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const source = Source.instances[0]!;
    source.status = 426;
    source.onerror?.();
    expect(source.closed).toBe(true);
    expect(states).not.toContain("schema-mismatch");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(Source.instances.length).toBeGreaterThan(1);
    expect(
      new URL(Source.instances[1]!.url, "http://studio.test").searchParams.get(
        "reconnect",
      ),
    ).toBe("true");
    stop();
    stopStatus();
  });

  it.each(["rollback", "unavailable"])(
    "checks the identity after a warm stream rejection (%s)",
    async (mode) => {
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
      await vi.advanceTimersByTimeAsync(20);
      expect(Source.instances).toHaveLength(1);
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
      await vi.advanceTimersByTimeAsync(20);
      expect(Source.instances).toHaveLength(1);
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
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
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
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    const windowListeners = new Map<string, () => void>();
    vi.stubGlobal("window", {
      addEventListener: vi.fn((name: string, listener: () => void) =>
        windowListeners.set(name, listener),
      ),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const first = Source.instances[0]!;
    first.open();
    await vi.advanceTimersByTimeAsync(15_000);
    first.emit("api-schema", { hash: API_SCHEMA_HASH });
    expect(first.closed).toBe(true);
    expect(states).not.toContain("schema-mismatch");
    for (let attempt = 2; attempt <= 3; attempt++) {
      Source.skipNextAutoHandshake = true;
      windowListeners.get("pageshow")?.();
      await vi.advanceTimersByTimeAsync(20);
      expect(Source.instances).toHaveLength(attempt);
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
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
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
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    syncDatabase.mockResolvedValue({ workspaceId });
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    Source.skipNextAutoHandshake = true;
    const transport = await loadTransport();
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges({ kind: "state" }, onChange);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    const transport = await loadTransport();
    const statuses: string[] = [];
    const streamStates: string[] = [];
    transport.watchResourceConnection((status, stream) => {
      statuses.push(status);
      streamStates.push(stream);
    });
    expect(streamStates[0]).toBe("stopped");
    const listener = vi.fn();
    const stop = transport.watchResourceChanges({ kind: "state" }, listener);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const source = Source.instances[0]!;
    expect(streamStates.at(-1)).toBe("connecting");
    source.open();
    expect(streamStates.at(-1)).toBe("open");
    source.emit("api-schema", { hash: API_SCHEMA_HASH });
    expect(() => source.emitRaw("resources", null)).not.toThrow();
    expect(source.closed).toBe(true);
    expect(statuses).toContain("degraded");
    await expectRecovered(listener);
    stop();
  });

  it("degrades and reconnects when a resource subscriber throws", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    const transport = await loadTransport();
    const statuses: string[] = [];
    transport.watchResourceConnection((status) => statuses.push(status));
    const listener = vi.fn().mockImplementationOnce(() => {
      throw new Error("subscriber failed");
    });
    const stop = transport.watchResourceChanges({ kind: "state" }, listener);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    await expectRecovered(listener);
    stop();
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
  ])("closes and degrades on %s", async (_label, eventName, frame) => {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    });
    const transport = await loadTransport();
    const states: string[] = [];
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const source = Source.instances[0]!;
    expect(() => source.emitRaw(eventName!, frame)).not.toThrow();
    expect(source.closed).toBe(true);
    expect(states).toContain("degraded");
    stop();
    stopStatus();
  });

  it("notifies only the matching resource once per revision and preserves liveness on reconfigure", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });

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
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const first = Source.instances[0]!;
    first.open();
    first.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [queue],
      resourceVersions: [{ resource: queue, revision: 1 }],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(queueChanges).toHaveBeenCalledTimes(1);
    first.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "change",
      resources: [queue],
      resourceVersions: [{ resource: queue, revision: 1 }],
    });
    await vi.advanceTimersByTimeAsync(30);
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
    await vi.advanceTimersByTimeAsync(40);
    expect(queueChanges).toHaveBeenCalledTimes(1);

    const beforeReconfigure = states.length;
    const stopTerminal = transport.watchResourceChanges(
      terminal,
      terminalChanges,
    );
    await vi.advanceTimersByTimeAsync(30);
    expect(subscriptionPosts).toHaveLength(1);
    expect(Source.instances).toHaveLength(1);
    const replacement = Source.instances[0]!;
    expect(
      states.slice(beforeReconfigure).every((state) => state === "live"),
    ).toBe(true);
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

  it("opens one owner stream when cursor reads take longer than its peer heartbeat", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const { readSubscriptionCursor } = await import("./client");
    vi.mocked(readSubscriptionCursor).mockImplementation(
      () =>
        new Promise<undefined>((resolve) =>
          setTimeout(() => resolve(undefined), 3_001),
        ),
    );
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
    const transport = await loadTransport();
    const changed = vi.fn();
    const stop = transport.watchResourceChanges({ kind: "state" }, changed);

    await vi.advanceTimersByTimeAsync(10_000);
    expect(Source.instances).toHaveLength(1);
    expect(readSubscriptionCursor).toHaveBeenCalledOnce();
    Source.instances[0]!.open();
    Source.instances[0]!.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "slow-cursor-owner",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "state" }],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(changed).toHaveBeenCalledOnce();
    stop();
  });

  it("fences a pending opening when its resource selection changes", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const { readSubscriptionCursor } = await import("./client");
    vi.mocked(readSubscriptionCursor).mockImplementation(
      () =>
        new Promise<undefined>((resolve) =>
          setTimeout(() => resolve(undefined), 3_001),
        ),
    );
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
    const transport = await loadTransport();
    const stopState = transport.watchResourceChanges(
      { kind: "state" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(1_000);
    const stopQueue = transport.watchResourceChanges(
      { kind: "queue", agentId: "later" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(2_500);
    expect(Source.instances).toHaveLength(0);
    // The changed selection restarts its cursor reads before one stream opens.
    await vi.advanceTimersByTimeAsync(4_000);
    expect(Source.instances).toHaveLength(1);
    expect(
      JSON.parse(
        new URL(Source.instances[0]!.url, location.origin).searchParams.get(
          "resources",
        )!,
      ),
    ).toEqual(
      expect.arrayContaining([
        { kind: "state" },
        { kind: "queue", agentId: "later" },
      ]),
    );
    // The state cursor read is reused; only the joining queue is read again.
    expect(readSubscriptionCursor).toHaveBeenCalledTimes(2);
    stopQueue();
    stopState();
  });

  it("an obsolete cursor read cannot release the resumed owner's pending opening", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const { readSubscriptionCursor } = await import("./client");
    vi.mocked(readSubscriptionCursor).mockImplementation(
      () =>
        new Promise<undefined>((resolve) =>
          setTimeout(() => resolve(undefined), 3_001),
        ),
    );
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
    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(1_000);
    const visibility = vi
      .mocked(document.addEventListener)
      .mock.calls.filter(([name]) => name === "visibilitychange")
      .at(-1)![1] as EventListener;
    Object.assign(document, { hidden: true });
    visibility(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(1_000);
    Object.assign(document, { hidden: false });
    visibility(new Event("visibilitychange"));
    await vi.advanceTimersByTimeAsync(4_000);
    expect(Source.instances).toHaveLength(1);
    expect(readSubscriptionCursor).toHaveBeenCalledTimes(2);
    stop();
  });

  it("keeps a quiet stream live across repeated server heartbeat intervals", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges(
      { kind: "queue", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
    vi.stubGlobal("navigator", navigatorStub);

    const transport = await loadTransport();
    const stop = transport.watchResourceChanges(
      { kind: "voice", agentId: "agent-one" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const first = Source.instances[0]!;
    first.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "voice", agentId: "agent-one" }],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(first.closed).toBe(false);

    navigatorStub.onLine = false;
    windowListeners.get("offline")?.();
    expect(first.closed).toBe(true);
    navigatorStub.onLine = true;
    windowListeners.get("online")?.();
    windowListeners.get("pageshow")?.();

    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(2);
    expect(Source.instances.filter((source) => !source.closed)).toHaveLength(1);
    stop();
  });

  it("cancels backoff offline and reconnects immediately online with one log per attempt", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const listeners = new Map<string, () => void>();
    vi.stubGlobal("window", {
      addEventListener: (name: string, listener: () => void) =>
        listeners.set(name, listener),
      removeEventListener: vi.fn(),
    });
    const transport = await loadTransport();
    const states = vi.fn();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    const stopConnection = transport.watchResourceConnection(states);
    await vi.advanceTimersByTimeAsync(20);
    Source.instances[0]!.onerror?.();
    expect(appendAppLog).toHaveBeenCalledTimes(1);
    expect(appendAppLog).toHaveBeenLastCalledWith(
      expect.objectContaining({
        title: "Reconnecting",
        reason: "Connection failed.",
      }),
    );
    listeners.get("offline")?.();
    expect(states).toHaveBeenLastCalledWith("offline", "offline", {});
    await vi.advanceTimersByTimeAsync(20_000);
    expect(Source.instances).toHaveLength(1);
    listeners.get("online")?.();
    expect(appendAppLog).toHaveBeenCalledTimes(2);
    expect(appendAppLog).toHaveBeenLastCalledWith(
      expect.objectContaining({
        title: "Reconnecting",
        reason: "The device is online again.",
      }),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(2);
    stopConnection();
    stop();
  });

  it("reconnects a restarted coordinator that missed the online event while stopped", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const listeners = new Map<string, () => void>();
    vi.stubGlobal("window", {
      addEventListener: (name: string, listener: () => void) =>
        listeners.set(name, listener),
      removeEventListener: (name: string) => listeners.delete(name),
    });
    const navigatorStub = { onLine: true };
    vi.stubGlobal("navigator", navigatorStub);
    const transport = await loadTransport();
    const stopFirst = transport.watchResourceChanges(
      { kind: "state" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    navigatorStub.onLine = false;
    listeners.get("offline")?.();
    stopFirst();
    await vi.advanceTimersByTimeAsync(2_000);
    expect(listeners.has("online")).toBe(false);
    // Connectivity returns while no coordinator listens for it.
    navigatorStub.onLine = true;
    const stopSecond = transport.watchResourceChanges(
      { kind: "state" },
      vi.fn(),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(2);
    stopSecond();
  });

  it("an online event interrupts an existing backoff", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const listeners = new Map<string, () => void>();
    vi.stubGlobal("window", {
      addEventListener: (name: string, listener: () => void) =>
        listeners.set(name, listener),
      removeEventListener: vi.fn(),
    });
    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    Source.instances[0]!.onerror?.();
    listeners.get("online")?.();
    expect(appendAppLog).toHaveBeenLastCalledWith(
      expect.objectContaining({ reason: "The device is online again." }),
    );
    await vi.advanceTimersByTimeAsync(1_000);
    expect(Source.instances).toHaveLength(2);
    stop();
  });

  it("cancels a retry when a status listener goes offline during scheduling", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
    const listeners = new Map<string, () => void>();
    vi.stubGlobal("window", {
      addEventListener: (name: string, listener: () => void) =>
        listeners.set(name, listener),
      removeEventListener: vi.fn(),
    });
    const transport = await loadTransport();
    const stop = transport.watchResourceChanges({ kind: "state" }, vi.fn());
    const stopConnection = transport.watchResourceConnection((status) => {
      if (status === "degraded") listeners.get("offline")?.();
    });
    await vi.advanceTimersByTimeAsync(20);
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    const schedule = vi.spyOn(globalThis, "setTimeout");
    const cancel = vi.spyOn(globalThis, "clearTimeout");
    Source.instances[0]!.onerror?.();
    const index = schedule.mock.calls.findIndex((call) => call[1] === 500);
    expect(index).toBeGreaterThanOrEqual(0);
    expect(cancel).toHaveBeenCalledWith(schedule.mock.results[index]!.value);
    await vi.advanceTimersByTimeAsync(1_000);
    expect(Source.instances).toHaveLength(1);
    stopConnection();
    stop();
  });

  it("treats valid busy-stream events as liveness and reconnects after silence", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });

    const transport = await loadTransport();
    const resource = { kind: "costs" } as const;
    const onChange = vi.fn();
    const states: string[] = [];
    const stopChanges = transport.watchResourceChanges(resource, onChange);
    const stopStatus = transport.watchResourceConnection((state) =>
      states.push(state),
    );
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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
  });

  it("delivers resource and token-rate events that share a hub revision", async () => {
    syncDatabase.mockResolvedValue({ workspaceId });
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

    const transport = await loadTransport();
    const resource = { kind: "costs" } as const;
    const onResourceChange = vi.fn();
    const onTokenRates = vi.fn();
    const stopResource = transport.watchResourceChanges(
      resource,
      onResourceChange,
    );
    const stopTokenRates = transport.watchTokenRateEvents(onTokenRates);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
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

    await vi.advanceTimersByTimeAsync(20);
    expect(onResourceChange).toHaveBeenCalledTimes(1);
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
    stopLate();
    stopTokenRates();
    stopResource();
  });

  it("reconciles an unchanged owner baseline once after reconnect", async () => {
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    const fetch = vi.fn().mockRejectedValue(new TypeError("Unexpected fetch"));
    vi.stubGlobal("fetch", fetch);
    syncDatabase.mockResolvedValue({ workspaceId });
    const api = await import("../api");
    const identityProbe = vi
      .spyOn(api, "get")
      .mockRejectedValue(new Error("Unexpected identity probe"));
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
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const local = { kind: "queue", agentId: "resume-queue" } as const;
    const onChange = vi.fn();
    const stop = transport.watchResourceChanges(local, onChange);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    // A warm reconnect follows an open stream, not a pre-open rejection.
    Source.instances[0]!.open();
    const initial = {
      protocol: 4,
      workspaceId,
      epoch: "epoch-one",
      revision: 7,
      reason: "initial",
      resources: [local],
      resourceVersions: [{ resource: local, revision: 7 }],
    };
    // An established stream opens before delivering its baseline. Without this,
    // onerror starts a real identity probe whose cleanup depends on network timing.
    Source.instances[0]!.open();
    Source.instances[0]!.emit("resources", initial);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(1);

    Source.instances[0]!.onerror?.();
    expect(fetch).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(500);
    expect(Source.instances).toHaveLength(2);
    Source.instances[1]!.open();
    Source.instances[1]!.emit("resources", {
      ...initial,
      reason: "reconnect",
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);

    Source.instances[1]!.emit("resources", initial);
    await vi.advanceTimersByTimeAsync(20);
    expect(onChange).toHaveBeenCalledTimes(2);

    expect(identityProbe).not.toHaveBeenCalled();
    stop();
  });

  it("recovers from a replaced stream with a new server epoch", async () => {
    const fetch = vi.fn().mockRejectedValue(new TypeError("Unexpected fetch"));
    vi.stubGlobal("fetch", fetch);
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    syncDatabase.mockResolvedValue({ workspaceId });
    const api = await import("../api");
    const identityProbe = vi
      .spyOn(api, "get")
      .mockRejectedValue(new Error("Unexpected identity probe"));

    const transport = await loadTransport();
    const resource = { kind: "state" } as const;
    const onChange = vi.fn();
    const states: string[] = [];
    const stopChanges = transport.watchResourceChanges(resource, onChange);
    const stopStatus = transport.watchResourceConnection((status) =>
      states.push(status),
    );
    try {
      await vi.advanceTimersByTimeAsync(20);
      expect(Source.instances).toHaveLength(1);
      const original = Source.instances[0]!;
      original.open();
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
      expect(fetch).not.toHaveBeenCalled();
      await vi.advanceTimersByTimeAsync(500);
      expect(Source.instances).toHaveLength(2);
      Source.instances[1]!.open();
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
      expect(onChange).toHaveBeenLastCalledWith(
        expect.objectContaining({ epoch: "server-after-restart", revision: 1 }),
      );
      expect(identityProbe).not.toHaveBeenCalled();
    } finally {
      stopChanges();
      stopStatus();
    }
  });

  it("drops a recovery marker when its last local subscriber leaves", async () => {
    vi.spyOn(Math, "random").mockReturnValue(0.5);
    const fetch = vi.fn().mockRejectedValue(new TypeError("Unexpected fetch"));
    vi.stubGlobal("fetch", fetch);
    syncDatabase.mockResolvedValue({ workspaceId });
    const api = await import("../api");
    const identityProbe = vi
      .spyOn(api, "get")
      .mockRejectedValue(new Error("Unexpected identity probe"));
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
    vi.stubGlobal("crypto", { randomUUID: () => "tab-owner" });
    vi.stubGlobal("BroadcastChannel", Channel);

    const transport = await loadTransport();
    const a = { kind: "queue", agentId: "resume-queue-a" } as const;
    const b = { kind: "queue", agentId: "resume-queue-b" } as const;
    const onA = vi.fn();
    const onB = vi.fn();
    let stopA = transport.watchResourceChanges(a, onA);
    const stopB = transport.watchResourceChanges(b, onB);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    Source.instances[0]!.open();

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
    // An established stream opens before its baseline; otherwise onerror starts
    // a real identity probe whose cleanup timer depends on network timing.
    Source.instances[0]!.open();
    Source.instances[0]!.emit("resources", baseline);
    await vi.advanceTimersByTimeAsync(20);
    expect(onA).toHaveBeenCalledTimes(1);
    expect(onB).toHaveBeenCalledTimes(1);

    Source.instances[0]!.onerror?.();
    expect(fetch).not.toHaveBeenCalled();
    stopA();
    await vi.advanceTimersByTimeAsync(5_000);
    expect(Source.instances).toHaveLength(2);
    Source.instances[1]!.open();
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
    expect(identityProbe).not.toHaveBeenCalled();
    stopA();
    stopB();
  });

  async function openNative() {
    syncDatabase.mockResolvedValue({ workspaceId });
    vi.stubGlobal("NativeTestSource", NativeSource);
    const transport = await loadTransport();
    const listener = vi.fn();
    const states: string[] = [];
    const streams: string[] = [];
    transport.watchResourceConnection((state, stream) => {
      states.push(state);
      streams.push(stream);
    });
    // Each native connection sends its own session and schema handshake.
    Source.skipNextAutoHandshake = true;
    transport.watchResourceChanges({ kind: "costs" }, listener);
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(1);
    const source = Source.instances[0] as NativeSource;
    source.open();
    source.emit("session", {
      sessionId: "session-one",
      workspaceId,
      routes: [],
    });
    source.emit("api-schema", { hash: API_SCHEMA_HASH });
    source.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "native",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "costs" }],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(states.at(-1)).toBe("live");
    return { source, listener, states, streams };
  }

  it("native stream waits for the core after a lost connection and answers its resume request", async () => {
    const { source, listener, states, streams } = await openNative();
    source.emit("heartbeat", { workspaceId, epoch: "native", revision: 1 });
    expect(appendAppLog).not.toHaveBeenCalled();
    source.onretry?.({ opened: true, retryAt: Date.now() + 5_000 });
    expect(states.at(-1)).toBe("degraded");
    expect(streams.at(-1)).toBe("retry-scheduled");
    // No web backoff, heartbeat watchdog or second stream runs on top.
    await vi.advanceTimersByTimeAsync(120_000);
    expect(Source.instances).toHaveLength(1);
    expect(source.closed).toBe(false);
    expect(source.restart).not.toHaveBeenCalled();
    expect(source.resume).not.toHaveBeenCalled();

    source.onresume?.();
    await vi.advanceTimersByTimeAsync(0);
    expect(source.resume).toHaveBeenCalledOnce();
    const path = new URL(
      String(source.resume.mock.calls[0]![0]),
      "http://studio.test",
    );
    expect(path.pathname).toBe("/api/sync/stream");
    expect(path.searchParams.get("reconnect")).toBe("true");
    expect(JSON.parse(path.searchParams.get("resources")!)).toEqual([
      { kind: "costs" },
    ]);
    source.open();
    source.emit("session", {
      sessionId: "session-two",
      workspaceId,
      routes: [],
    });
    source.emit("api-schema", { hash: API_SCHEMA_HASH });
    source.emit("resources", {
      protocol: 4,
      workspaceId,
      epoch: "native",
      revision: 2,
      reason: "reconnect",
      resources: [{ kind: "costs" }],
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(states.at(-1)).toBe("live");
    expect(listener).toHaveBeenLastCalledWith(
      expect.objectContaining({ revision: 2 }),
    );
    expect(Source.instances).toHaveLength(1);
  });

  it("native stream asks the core to restart a connection the page cannot use", async () => {
    const { source, states } = await openNative();
    source.emitRaw("resources", null);
    expect(source.restart).toHaveBeenCalledOnce();
    expect(String(source.restart.mock.calls[0]![0])).toMatch(/^invalid-frame/);
    expect(source.closed).toBe(false);
    expect(states.at(-1)).toBe("degraded");
    expect(appendAppLog).toHaveBeenCalledOnce();
    expect(appendAppLog).toHaveBeenLastCalledWith(
      expect.objectContaining({ reason: "Received invalid server updates." }),
    );
    source.onretry?.({ opened: false, retryAt: Date.now() + 5_000 });
    expect(appendAppLog).toHaveBeenCalledOnce();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(Source.instances).toHaveLength(1);
    expect(source.restart).toHaveBeenCalledOnce();
  });
});

describe("connection owner: retries, wakeups and stream probes", () => {
  afterEach(async () => {
    for (const stop of activeStops) stop();
    await vi.advanceTimersByTimeAsync(0);
    const api = await import("../api");
    api.markApiSchemaMismatch();
    // Drain deferred coordinator shutdown without assuming a fixed timer deadline.
    // The assertion still rejects recurring timers after cleanup.
    await vi.runOnlyPendingTimersAsync();
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
  });

  const resource = { kind: "state" } as const;
  const baseline = (epoch: string) => ({
    protocol: 4,
    workspaceId,
    epoch,
    revision: 1,
    reason: "initial",
    resources: [resource],
  });

  async function open() {
    syncDatabase.mockResolvedValue({ workspaceId });
    const windowListeners = new Map<string, () => void>();
    vi.stubGlobal("window", {
      addEventListener: (name: string, listener: () => void) =>
        windowListeners.set(name, listener),
      removeEventListener: vi.fn(),
    });
    const transport = await loadTransport();
    const details: ResourceConnectionDetail[] = [];
    const health: string[] = [];
    transport.watchResourceConnection((status, _activity, detail) => {
      health.push(status);
      details.push(detail);
    });
    transport.watchResourceChanges(resource, vi.fn());
    await vi.advanceTimersByTimeAsync(20);
    let epoch = "";
    const goLive = async () => {
      const source = Source.instances.at(-1)!;
      epoch = `epoch-${Source.instances.length}`;
      source.open();
      source.emit("resources", baseline(epoch));
      await vi.advanceTimersByTimeAsync(20);
      return source;
    };
    /** Keeps the stream healthy with heartbeats while time passes. */
    const stayUp = async (ms: number) => {
      for (let passed = 0; passed < ms; passed += 20_000) {
        Source.instances
          .at(-1)!
          .emit("heartbeat", { protocol: 4, workspaceId, epoch, revision: 1 });
        await vi.advanceTimersByTimeAsync(Math.min(20_000, ms - passed));
      }
    };
    return { windowListeners, health, details, goLive, stayUp };
  }

  /** The delay of the retry that the last failure scheduled. */
  const lastDelay = (details: ResourceConnectionDetail[]) => {
    const retryAt = details.at(-1)?.retryAt;
    return retryAt === undefined ? undefined : retryAt - Date.now();
  };

  it("backs off with jitter up to a 5 minute cap", async () => {
    vi.spyOn(Math, "random").mockReturnValue(0.999);
    const { details } = await open();
    let longest = 0;
    for (let i = 0; i < 14; i++) {
      Source.instances.at(-1)!.onerror?.();
      const delay = lastDelay(details)!;
      expect(delay).toBeLessThanOrEqual(5 * 60_000);
      longest = Math.max(longest, delay);
      await vi.advanceTimersByTimeAsync(delay + 20);
    }
    expect(longest).toBe(5 * 60_000);
    vi.spyOn(Math, "random").mockReturnValue(0);
    Source.instances.at(-1)!.onerror?.();
    expect(lastDelay(details)).toBeLessThan(5 * 60_000);
  });

  it("resets the retry count only after the stream stays up", async () => {
    const { details, goLive, stayUp } = await open();
    const failAndRecover = async (liveFor: number) => {
      Source.instances.at(-1)!.onerror?.();
      const attempt = details.at(-1)!.attempt;
      await vi.advanceTimersByTimeAsync(lastDelay(details)! + 20);
      await goLive();
      await stayUp(liveFor);
      return attempt;
    };
    await goLive();
    expect(await failAndRecover(10_000)).toBe(1);
    // It was live for 10 s only, so the next failure continues the count.
    expect(await failAndRecover(10_000)).toBe(2);
    expect(await failAndRecover(120_000)).toBe(3);
    // After 2 minutes live the count starts again.
    expect(await failAndRecover(10_000)).toBe(1);
  });

  it("waits for a wakeup after the server refuses the device", async () => {
    const { windowListeners, details, goLive } = await open();
    await goLive();
    const refused = Source.instances.at(-1)!;
    refused.onerror?.({ status: 403 });
    expect(details.at(-1)).toEqual(
      expect.objectContaining({ failure: "auth-rejected" }),
    );
    expect(details.at(-1)!.retryAt).toBeUndefined();
    await vi.advanceTimersByTimeAsync(30 * 60_000);
    expect(Source.instances).toHaveLength(1);
    windowListeners.get("pageshow")?.();
    await vi.advanceTimersByTimeAsync(20);
    expect(Source.instances).toHaveLength(2);
  });

  it("probes an open stream on foreground and keeps it when the server answers", async () => {
    const { windowListeners, goLive, stayUp } = await open();
    const stream = await goLive();
    subscriptionPosts.length = 0;
    windowListeners.get("pageshow")?.();
    await vi.advanceTimersByTimeAsync(100);
    expect(subscriptionPosts).toHaveLength(1);
    expect(subscriptionPosts[0]).toEqual(
      expect.objectContaining({ add: [], remove: [] }),
    );
    await stayUp(60_000);
    expect(stream.closed).toBe(false);
    expect(Source.instances).toHaveLength(1);
  });

  it("replaces the stream when the probe gets no answer in time", async () => {
    const api = await import("../api");
    const { windowListeners, goLive } = await open();
    const stream = await goLive();
    vi.spyOn(api, "post").mockImplementation(async () => new Promise(() => {}));
    windowListeners.get("pageshow")?.();
    await vi.advanceTimersByTimeAsync(4_900);
    expect(stream.closed).toBe(false);
    await vi.advanceTimersByTimeAsync(200);
    expect(stream.closed).toBe(true);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(Source.instances).toHaveLength(2);
  });

  it("probes instead of replacing the stream when the network comes back", async () => {
    const { windowListeners, goLive } = await open();
    const stream = await goLive();
    subscriptionPosts.length = 0;
    windowListeners.get("online")?.();
    await vi.advanceTimersByTimeAsync(100);
    expect(stream.closed).toBe(false);
    expect(subscriptionPosts).toHaveLength(1);
  });

  it("replaces the stream when the route changes", async () => {
    const { goLive } = await open();
    const stream = await goLive();
    const transport = await import("./resourceEvents");
    transport.replaceStream();
    expect(stream.closed).toBe(true);
    await vi.advanceTimersByTimeAsync(2_000);
    expect(Source.instances).toHaveLength(2);
  });
});
