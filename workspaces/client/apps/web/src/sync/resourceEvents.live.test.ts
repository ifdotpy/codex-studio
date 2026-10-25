import { afterEach, expect, it, vi } from "vitest";
import { createResourceEvents } from "./resourceEvents";
import {
  resourceCoordinationNames,
  RESOURCE_COORDINATION_VERSION,
  RESOURCE_CURSOR_PREPARATION_TIMEOUT_MS,
} from "./resourceConnection";
import { createSubscriptionStore } from "./subscriptions/store";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
import { SYNC_STREAM_EVENT_MAX_BYTES } from "../generated/sharedValues";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";

vi.mock("./client", () => ({ syncDatabase: vi.fn() }));
vi.mock("../usage/tokenRate", () => ({
  configureTokenRateStream: vi.fn(),
  receiveResourceTokenRates: vi.fn(),
  getWorkspaceTokenRates: vi.fn(() => ({ records: [] })),
}));

vi.mock("./entityTransfer", async (original) => {
  const module = await original<typeof import("./entityTransfer")>();
  return {
    ...module,
    EntityTransferAssembler: class extends module.EntityTransferAssembler {
      constructor() {
        const rows = new Map<string, string>();
        super({
          async put(key, value) {
            rows.set(key, value);
          },
          async putMany(values) {
            for (const [key, value] of values) rows.set(key, value);
          },
          async get(key) {
            return rows.get(key);
          },
          async remove(prefix) {
            for (const key of rows.keys())
              if (key.startsWith(prefix)) rows.delete(key);
          },
          async close() {
            rows.clear();
          },
        });
      }
    },
  };
});

class Source {
  listeners = new Map<string, (event: Event) => void>();
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  addEventListener(name: string, listener: (event: Event) => void) {
    this.listeners.set(name, listener);
  }
  close() {
    this.closed = true;
  }
  emit(name: string, value: unknown) {
    this.listeners.get(name)?.({
      data: JSON.stringify(value),
    } as MessageEvent<string>);
  }
}
const disposals: (() => void)[] = [];
afterEach(() => {
  for (const dispose of disposals.splice(0)) dispose();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function fixture(
  follower = false,
  session = true,
  peerOwner = false,
  baseline = true,
) {
  vi.useFakeTimers();
  const browser = new EventTarget();
  const page = Object.assign(new EventTarget(), { hidden: false });
  vi.spyOn(browser, "addEventListener");
  vi.spyOn(browser, "removeEventListener");
  vi.spyOn(page, "addEventListener");
  vi.spyOn(page, "removeEventListener");
  vi.stubGlobal("window", browser);
  vi.stubGlobal("document", page);
  const lockNames: string[] = [];
  const channelNames: string[] = [];
  let acquire: ((lock: Lock | null) => Promise<void>) | undefined;
  vi.stubGlobal("navigator", {
    onLine: true,
    ...(follower || peerOwner
      ? {
          locks: {
            request: (
              _name: string,
              _options: { signal: AbortSignal },
              callback: (lock: Lock | null) => Promise<void>,
            ) => {
              lockNames.push(_name);
              acquire = callback;
              if (peerOwner)
                return callback({ name: "fixture", mode: "exclusive" } as Lock);
              // A normal exclusive request queues while another owner holds
              // the lock. It never invokes the callback with a null lock.
              return new Promise<void>((resolve, reject) => {
                acquire = (lock) => callback(lock).then(resolve, reject);
                _options.signal.addEventListener(
                  "abort",
                  () => reject(new DOMException("Aborted", "AbortError")),
                  { once: true },
                );
              });
            },
          },
        }
      : {}),
  });
  vi.stubGlobal("location", { origin: "http://studio.test" });
  let deliverPeer: ((event: MessageEvent) => void) | undefined;
  const posted: any[] = [];
  vi.stubGlobal(
    "BroadcastChannel",
    follower || peerOwner
      ? class {
          onmessage: ((event: MessageEvent) => void) | null = null;
          constructor(name: string) {
            channelNames.push(name);
            deliverPeer = (event) => this.onmessage?.(event);
          }
          postMessage(message: unknown) {
            posted.push(structuredClone(message));
          }
          close() {}
        }
      : undefined,
  );
  let peerSequence = 0;
  const peer = (name: string, event: unknown) =>
    deliverPeer?.({
      data: {
        workspaceId: "workspace",
        tabId: "owner",
        apiSchemaHash: API_SCHEMA_HASH,
        kind: name === "token-rates" ? name : "resource-event",
        health: health("live", 1, peerSequence++),
        event,
      },
    } as MessageEvent);
  const control = (kind: string, fields: Record<string, unknown>) =>
    deliverPeer?.({
      data: {
        workspaceId: "workspace",
        tabId: "owner",
        apiSchemaHash: API_SCHEMA_HASH,
        kind,
        ...fields,
      },
    } as MessageEvent);
  const sources: Source[] = [];
  const urls: string[] = [];
  const post = vi.fn().mockResolvedValue({ gen: 1 });
  const get = vi.fn().mockResolvedValue({});
  const readCursor = vi.fn().mockResolvedValue(undefined);
  const persist = vi.fn().mockResolvedValue(undefined);
  const staged = vi.fn().mockResolvedValue(undefined);
  const transport = createResourceEvents({
    api: {
      get,
      post,
      clearSchemaUpdateAttemptAfterMatch: vi.fn(),
      isApiSchemaMismatch: () => false,
      matchingApiSchemaResponseGeneration: () => 0,
      markApiSchemaMismatch: vi.fn(),
      onApiSchemaMismatch: () => () => {},
    },
    sync: {
      syncDatabase: vi.fn().mockResolvedValue({ workspaceId: "workspace" }),
      persistResourceEntityChanges: persist,
      persistStagedEntityTransfer: staged,
      readSubscriptionCursor: readCursor,
    },
    tokenRates: {
      configureTokenRateStream: vi.fn(),
      receiveResourceTokenRates: vi.fn(() => true),
      getWorkspaceTokenRates: vi.fn(() => ({ records: [] })),
    },
    openEvents: (url) => {
      const source = new Source();
      sources.push(source);
      urls.push(url);
      return source;
    },
    storageName: (name) => name,
  });
  disposals.push(transport.dispose);
  const state = { kind: "state" } as const;
  const stopState = transport.watchResourceChanges(state, vi.fn());
  await vi.advanceTimersByTimeAsync(0);
  const source = follower ? ({ emit: peer } as Source) : sources[0]!;
  if (!follower && session)
    source.emit("session", { sessionId: "session-one" });
  if (!follower) source.emit("api-schema", { hash: API_SCHEMA_HASH });
  if (baseline)
    source.emit("resources", {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision: 1,
      reason: "initial",
      resources: [state],
      resourceVersions: [{ revision: 1 }],
    });
  await vi.advanceTimersByTimeAsync(25);
  return {
    transport,
    lockNames,
    channelNames,
    source,
    sources,
    urls,
    post,
    persist,
    staged,
    stopState,
    get,
    readCursor,
    posted,
    peerMessage: (message: Record<string, unknown>) =>
      deliverPeer?.({
        data: {
          ...message,
          workspaceId: "workspace",
          apiSchemaHash: API_SCHEMA_HASH,
          tabId: "late-follower",
        },
      } as MessageEvent),
    takeOwnership: () => {
      void acquire?.({ name: "fixture", mode: "exclusive" } as Lock);
    },
    messages: posted,
    control,
  };
}

it("publishes each resource event without a listener flush timer", async () => {
  const f = await fixture();
  const listener = vi.fn();
  f.transport.watchResourceChanges({ kind: "state" }, listener);
  listener.mockClear();
  for (const revision of [2, 3]) {
    f.source.emit("resources", {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision,
      reason: "change",
      resources: [{ kind: "state" }],
      resourceVersions: [{ revision }],
    });
    // Drain event persistence microtasks without advancing any timer.
    for (let turn = 0; turn < 10; turn++) await Promise.resolve();
    expect(listener).toHaveBeenCalledTimes(revision - 1);
    expect(listener).toHaveBeenLastCalledWith(
      expect.objectContaining({ revision }),
    );
  }
});

it("confirms a late store from an already active applied baseline without another read", async () => {
  const f = await fixture();
  f.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    reason: "initial",
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: 2,
        cursor: { workspaceId: "workspace", seq: 2 },
        replyKind: "snapshot",
        entityChanges: { after: 0, through: 2, documents: [] },
      },
    ],
  });
  await vi.advanceTimersByTimeAsync(0);
  const store = createSubscriptionStore(f.transport);
  disposals.push(store.dispose);
  store.watchResourceChanges({ kind: "state" }, () => {});
  await vi.advanceTimersByTimeAsync(0);
  expect(store.getSnapshot({ kind: "state" }).status).toBe("live");
  expect(f.get).not.toHaveBeenCalled();
  expect(f.sources).toHaveLength(1);
});

it("fences an active baseline replay when its stream fails during registration", async () => {
  const f = await fixture();
  f.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    reason: "initial",
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: 2,
        cursor: { workspaceId: "workspace", seq: 2 },
        replyKind: "snapshot",
        entityChanges: { after: 0, through: 2, documents: [] },
      },
    ],
  });
  await vi.advanceTimersByTimeAsync(0);
  const store = createSubscriptionStore(f.transport);
  disposals.push(store.dispose);
  store.watchResourceChanges({ kind: "state" }, () => {});
  f.source.onerror?.();
  await vi.advanceTimersByTimeAsync(0);
  expect(store.getSnapshot({ kind: "state" }).status).toBe("stale");
  expect(f.get.mock.calls.map(([path]) => path)).toEqual([
    "/api/sync/identity",
  ]);
});

it("promotes a same-revision recovery hint only after its applied receipt arrives", async () => {
  const f = await fixture();
  const store = createSubscriptionStore(f.transport);
  disposals.push(store.dispose);
  store.watchResourceChanges({ kind: "state" }, () => {});
  expect(store.getSnapshot({ kind: "state" }).status).toBe("loading");
  f.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 1,
    reason: "reconnect",
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: 1,
        cursor: { workspaceId: "workspace", seq: 1 },
        replyKind: "delta",
      },
    ],
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(store.getSnapshot({ kind: "state" }).status).toBe("live");
  expect(f.get).not.toHaveBeenCalled();
});

it("revokes historical same-revision proof when the current reconnect application fails", async () => {
  const f = await fixture();
  const baseline = {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    reason: "initial",
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: 2,
        cursor: { workspaceId: "workspace", seq: 2 },
        replyKind: "delta",
      },
    ],
  };
  f.source.emit("resources", baseline);
  await vi.advanceTimersByTimeAsync(0);
  const store = createSubscriptionStore(f.transport);
  disposals.push(store.dispose);
  store.watchResourceChanges({ kind: "state" }, () => {});
  await vi.advanceTimersByTimeAsync(0);
  expect(store.getSnapshot({ kind: "state" }).status).toBe("live");
  f.source.onerror?.();
  expect(store.getSnapshot({ kind: "state" }).status).toBe("stale");
  await vi.advanceTimersByTimeAsync(1000);
  const resumed = f.sources.at(-1)!;
  resumed.emit("api-schema", { hash: API_SCHEMA_HASH });
  f.persist.mockRejectedValueOnce(
    new Error("the current local base is unavailable"),
  );
  const error = vi.spyOn(console, "error").mockImplementation(() => {});
  resumed.emit("resources", { ...baseline, reason: "reconnect" });
  await vi.advanceTimersByTimeAsync(0);
  expect(store.getSnapshot({ kind: "state" }).status).not.toBe("live");
  error.mockRestore();
});

it("reports a deferred baseline listener failure through normal transport recovery", async () => {
  const f = await fixture();
  f.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    reason: "initial",
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: 2,
        cursor: { workspaceId: "workspace", seq: 2 },
        replyKind: "snapshot",
        entityChanges: { after: 0, through: 2, documents: [] },
      },
    ],
  });
  await vi.advanceTimersByTimeAsync(0);
  const error = vi.spyOn(console, "error").mockImplementation(() => {});
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  const listener = vi
    .fn()
    .mockImplementationOnce(() => {})
    .mockImplementation(() => {
      throw new Error("listener unavailable");
    });
  f.transport.watchResourceChanges({ kind: "state" }, listener, {
    waitForBaseline: true,
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(status).toHaveBeenCalledWith(
    "degraded",
    expect.any(String),
    expect.objectContaining({ failure: "local-state-error" }),
  );
  error.mockRestore();
});

it("delivers subscribed auth progress without sending its URL to the entity persister", async () => {
  const f = await fixture();
  const resource = { kind: "auth-progress", requestId: "login-one" } as const;
  const listener = vi.fn();
  f.transport.watchResourceChanges(resource, listener);
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  listener.mockClear();
  f.persist.mockClear();
  const value = {
    requestId: "login-one",
    accountKey: "claude-one",
    status: "pending",
    verificationUrl: "https://claude.com/cai/oauth/authorize?secret=fixture",
  };
  f.source.emit("resource-progress", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    resource,
    value,
  });
  expect(listener).toHaveBeenCalledWith(
    expect.objectContaining({ progress: { resource, value } }),
  );
  expect(f.persist).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(10_000);
  expect(f.post).toHaveBeenCalledTimes(1);
});

it("delivers vault job codes only through transient subscribed frames", async () => {
  const f = await fixture();
  const resource = {
    kind: "vault-job-progress",
    requestId: "vault-one",
    job: "login",
  } as const;
  const listener = vi.fn();
  f.transport.watchResourceChanges(resource, listener);
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  f.persist.mockClear();
  const value = {
    requestId: "vault-one",
    state: "running",
    page: "https://device.example/transient",
    code: "TEMP",
  };
  f.source.emit("resource-progress", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    resource,
    value,
  });
  expect(listener).toHaveBeenCalledWith(
    expect.objectContaining({ progress: { resource, value } }),
  );
  expect(f.persist).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(10_000);
  expect(f.post).toHaveBeenCalledTimes(1);
});

it("coalesces chat switches into one POST without reopening the SSE stream", async () => {
  const f = await fixture();
  for (let i = 0; i < 10; i++) {
    const stop = f.transport.watchResourceChanges(
      { kind: "transcript", agentId: `chat-${i}` },
      vi.fn(),
    );
    stop();
  }
  f.transport.watchResourceChanges(
    { kind: "transcript", agentId: "chat-final" },
    vi.fn(),
  );
  await vi.advanceTimersByTimeAsync(50);
  expect(f.sources).toHaveLength(1);
  expect(f.post).toHaveBeenCalledTimes(1);
  expect(f.post.mock.calls[0]?.[1]).toMatchObject({
    sessionId: "session-one",
    gen: 1,
    add: [{ resource: { kind: "transcript", agentId: "chat-final" } }],
    remove: [],
  });
});

it("reopens with the full desired set when a stream has not announced its session", async () => {
  const f = await fixture(false, false);
  const transcript = { kind: "transcript", agentId: "chat" } as const;
  f.transport.watchResourceChanges(transcript, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  expect(f.source.closed).toBe(true);
  expect(f.post).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1000);
  expect(f.sources).toHaveLength(2);
  expect(
    JSON.parse(
      new URL(f.urls[1]!, "http://studio.test").searchParams.get("resources")!,
    ),
  ).toEqual([{ kind: "state" }, transcript]);
});

it("keeps a session-less stream when its desired resources are unchanged", async () => {
  const f = await fixture(false, false);
  f.transport.watchResourceChanges({ kind: "state" }, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  expect(f.source.closed).toBe(false);
  expect(f.sources).toHaveLength(1);
  expect(f.post).not.toHaveBeenCalled();
});

it("drops removed data and waits for acknowledgement before accepting a re-added resource", async () => {
  const f = await fixture();
  const resource = { kind: "queue", agentId: "chat" } as const;
  const oldListener = vi.fn();
  const stop = f.transport.watchResourceChanges(resource, oldListener);
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  const frame = (revision: number) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision,
    reason: "change",
    resources: [resource],
    resourceVersions: [{ revision }],
  });
  f.source.emit("resources", frame(2));
  await vi.advanceTimersByTimeAsync(25);
  expect(oldListener).toHaveBeenCalledTimes(1);
  stop();
  await vi.advanceTimersByTimeAsync(50);
  const listener = vi.fn();
  f.transport.watchResourceChanges(resource, listener);
  listener.mockClear();
  f.source.emit("resources", frame(3));
  await vi.advanceTimersByTimeAsync(25);
  expect(listener).not.toHaveBeenCalled();
  f.source.emit("subscriptions", { gen: 2 });
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("resources", frame(4));
  await vi.advanceTimersByTimeAsync(25);
  expect(listener).not.toHaveBeenCalled();
  f.source.emit("subscriptions", { gen: 3 });
  f.source.emit("resources", frame(5));
  await vi.advanceTimersByTimeAsync(25);
  expect(listener).toHaveBeenCalledTimes(1);
  expect(listener).toHaveBeenLastCalledWith(
    expect.objectContaining({ revision: 5 }),
  );
});

it("reopens with the full desired set when the session expired", async () => {
  const f = await fixture();
  f.post.mockRejectedValueOnce(
    Object.assign(new Error("expired"), { status: 410 }),
  );
  f.transport.watchResourceChanges({ kind: "queue", agentId: "chat" }, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  expect(f.post).toHaveBeenCalledTimes(1);
  expect(f.source.closed).toBe(true);
  await vi.advanceTimersByTimeAsync(1000);
  expect(f.sources).toHaveLength(2);
  expect(
    JSON.parse(
      new URL(f.urls[1]!, "http://studio.test").searchParams.get("resources")!,
    ),
  ).toHaveLength(2);
});

it("cancels queued data on removal even if persistence finishes after re-add ack", async () => {
  const f = await fixture();
  const resource = { kind: "transcript", agentId: "slow" } as const;
  const stop = f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  let finish!: () => void;
  f.persist.mockImplementationOnce(
    () =>
      new Promise<void>((resolve) => {
        finish = resolve;
      }),
  );
  const frame = (revision: number) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision,
    reason: "change",
    resources: [resource],
    resourceVersions: [
      {
        revision,
        transcriptChanges: {
          after: revision - 1,
          through: revision,
          delta: { delta: true, items: [], removed: [], truncated: false },
        },
      },
    ],
  });
  f.source.emit("resources", frame(2));
  f.source.emit("resources", frame(3));
  expect(f.persist).toHaveBeenCalledTimes(1);
  stop();
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 2 });
  const listener = vi.fn();
  f.transport.watchResourceChanges(resource, listener);
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 3 });
  listener.mockClear();
  finish();
  await vi.advanceTimersByTimeAsync(25);
  expect(f.persist).toHaveBeenCalledTimes(1);
  expect(listener).not.toHaveBeenCalled();
});

it("drops initial data for an addition removed before its acknowledgement", async () => {
  const f = await fixture();
  const resource = {
    kind: "transcript",
    agentId: "removed-during-add",
  } as const;
  const stop = f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  stop();
  f.source.emit("subscriptions", { gen: 1 });
  f.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    reason: "initial",
    resources: [resource],
    resourceVersions: [{ revision: 2 }],
  });
  await vi.advanceTimersByTimeAsync(25);
  const listener = vi.fn();
  f.transport.watchResourceChanges(resource, listener);
  expect(listener).not.toHaveBeenCalled();
});

it("a late follower ignores deltas until the same-revision full token replay", async () => {
  const f = await fixture(true);
  const listener = vi.fn();
  f.transport.watchTokenRateEvents(listener);
  const record = {
    agentId: "active",
    rootId: "root",
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 1,
    outputTokens: 10,
  };
  const frame = {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 7,
    snapshot: false,
    records: [record],
    removed: [],
  };
  f.source.emit("token-rates", frame);
  expect(listener).not.toHaveBeenCalled();
  const full = {
    ...frame,
    snapshot: true,
    records: [record, { ...record, agentId: "quiet", active: false, rate: 0 }],
  };
  f.source.emit("token-rates", full);
  expect(listener).toHaveBeenCalledExactlyOnceWith(full);
  f.source.emit("token-rates", full);
  expect(listener).toHaveBeenCalledTimes(1);
  f.source.emit("token-rates", { ...frame, epoch: "new", revision: 1 });
  expect(listener).toHaveBeenCalledTimes(1);
  f.source.emit("token-rates", { ...full, epoch: "new", revision: 1 });
  expect(listener).toHaveBeenCalledTimes(2);
});

it("invalidates pending follower persistence when its last transcript subscriber leaves", async () => {
  const f = await fixture(true);
  const resource = { kind: "transcript", agentId: "slow-follower" } as const;
  const stop = f.transport.watchResourceChanges(resource, vi.fn());
  let finish!: () => void;
  f.persist.mockImplementationOnce(
    () =>
      new Promise<void>((resolve) => {
        finish = resolve;
      }),
  );
  const frame = (revision: number) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision,
    reason: "change",
    resources: [resource],
    resourceVersions: [
      {
        revision,
        transcriptChanges: {
          after: revision - 1,
          through: revision,
          delta: { delta: true, items: [], removed: [], truncated: false },
        },
      },
    ],
  });
  f.source.emit("resources", frame(2));
  f.source.emit("resources", frame(3));
  expect(f.persist).toHaveBeenCalledTimes(1);
  const isCurrent = f.persist.mock.calls[0]![1] as () => boolean;
  expect(isCurrent()).toBe(true);
  stop();
  expect(isCurrent()).toBe(false);
  finish();
  await vi.advanceTimersByTimeAsync(25);
  expect(f.persist).toHaveBeenCalledTimes(1);
});

it.each(["entityChanges", "transcriptChanges"])(
  "rejects oversized UTF-8 %s frame data before parsing or persistence",
  async (batch) => {
    const f = await fixture();
    f.persist.mockClear();
    const data = JSON.stringify({
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision: 2,
      reason: "change",
      resources: [{ kind: "state" }],
      resourceVersions: [
        {
          revision: 2,
          [batch]: { text: "é".repeat(SYNC_STREAM_EVENT_MAX_BYTES / 2) },
        },
      ],
    });
    expect(data.length).toBeLessThan(SYNC_STREAM_EVENT_MAX_BYTES);
    expect(new TextEncoder().encode(data).length).toBeGreaterThan(
      SYNC_STREAM_EVENT_MAX_BYTES,
    );
    const parse = vi.spyOn(JSON, "parse");
    try {
      f.source.listeners.get("resources")?.({ data } as MessageEvent<string>);
      expect(parse).not.toHaveBeenCalledWith(data);
      expect(f.persist).not.toHaveBeenCalled();
      expect(f.source.closed).toBe(true);
    } finally {
      parse.mockRestore();
    }
  },
);

it.each(["resources", "token-rates"])(
  "counts received resources bytes once and exempts %s when appropriate",
  async (type) => {
    const f = await fixture();
    const frame = JSON.stringify({
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision: 2,
      reason: "change",
      resources: [{ kind: "state" }],
      resourceVersions: [
        { revision: 2, entityChanges: { after: 1, through: 2, documents: [] } },
      ],
      snapshot: true,
      records: [],
      removed: [],
    });
    const size = SYNC_STREAM_EVENT_MAX_BYTES + (type === "token-rates" ? 1 : 0);
    const data = frame.padEnd(size, " ");
    const encode = vi.spyOn(TextEncoder.prototype, "encode");
    const parse = vi.spyOn(JSON, "parse");
    try {
      f.source.listeners.get(type)?.({ data } as MessageEvent<string>);
      expect(parse).toHaveBeenCalledWith(data);
      expect(encode).toHaveBeenCalledTimes(type === "resources" ? 1 : 0);
      if (type === "resources") expect(encode).toHaveBeenCalledWith(data);
      expect(f.source.closed).toBe(false);
    } finally {
      encode.mockRestore();
      parse.mockRestore();
    }
  },
);

it("sends applied ref cursors on re-add and reconnect with zero pulls", async () => {
  const f = await fixture();
  const resource = { kind: "transcript", agentId: "recent" } as const;
  const cursor = { workspaceId: "workspace", seq: 41 };
  f.readCursor.mockImplementation(async (ref) =>
    ref.kind === "transcript" ? cursor : undefined,
  );
  const stop = f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  expect(f.post).toHaveBeenLastCalledWith(
    "/api/sync/subscriptions",
    expect.objectContaining({ add: [{ resource, since: cursor }] }),
    expect.anything(),
  );
  f.source.emit("subscriptions", { gen: 1 });
  stop();
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 2 });
  f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  expect(f.post).toHaveBeenLastCalledWith(
    "/api/sync/subscriptions",
    expect.objectContaining({ add: [{ resource, since: cursor }] }),
    expect.anything(),
  );
  f.source.onerror?.();
  await vi.advanceTimersByTimeAsync(10000);
  const url = new URL(f.urls.at(-1)!, "http://studio.test");
  expect(JSON.parse(url.searchParams.get("since")!)).toContainEqual({
    resource,
    since: cursor,
  });
  expect(
    f.get.mock.calls.some(([path]) => String(path).includes("/api/sync/pull")),
  ).toBe(false);
});

it.each([false, true])(
  "recovers an unchanged transcript baseline after transport loss (hidden=%s)",
  async (hidden) => {
    const f = await fixture();
    const store = createSubscriptionStore(f.transport);
    disposals.push(store.dispose);
    const resource = { kind: "transcript", agentId: "recovered-chat" } as const;
    const status = vi.fn();
    const stopStatus = f.transport.watchResourceConnection(status);
    const cursor = { workspaceId: "workspace", seq: 41 };
    f.readCursor.mockResolvedValue(cursor);
    let stop = store.watchResourceChanges(resource, vi.fn());
    await vi.advanceTimersByTimeAsync(50);
    f.source.emit("subscriptions", { gen: 1 });
    const baseline = {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision: 41,
      reason: "initial",
      resources: [resource],
      resourceVersions: [{ revision: 41, cursor, replyKind: "delta" }],
    };
    f.source.emit("resources", baseline);
    await vi.advanceTimersByTimeAsync(0);
    expect(store.getSnapshot(resource).status).toBe("live");
    f.source.onerror?.();
    expect(store.getSnapshot(resource).status).toBe("stale");
    if (hidden) stop();
    await vi.advanceTimersByTimeAsync(1000);
    const resumed = f.sources.at(-1)!;
    expect(resumed).not.toBe(f.source);
    const lateStatus = vi.fn();
    const stopLateStatus = f.transport.watchResourceConnection(lateStatus);
    expect(lateStatus).toHaveBeenLastCalledWith(
      "degraded",
      "connecting",
      expect.objectContaining({ failure: "stream-error" }),
    );
    resumed.emit("api-schema", { hash: API_SCHEMA_HASH });
    resumed.emit("resources", { ...baseline, reason: "reconnect" });
    await vi.advanceTimersByTimeAsync(0);
    if (hidden) {
      expect(store.getSnapshot(resource).status).toBe("stale");
      stop = store.watchResourceChanges(resource, vi.fn());
    }
    expect(store.getSnapshot(resource).status).toBe("live");
    expect(f.get.mock.calls.map(([path]) => path)).toEqual([
      "/api/sync/identity",
    ]);
    stop();
    stopStatus();
    stopLateStatus();
  },
);

it("T28 recently viewed chat switching sends no subscription changes within grace", async () => {
  const measurements: {
    mode: string;
    switches: number;
    subscriptionPosts: number;
  }[] = [];
  for (const mode of ["immediate", "grace"] as const) {
    const f = await fixture();
    const store = createSubscriptionStore(f.transport);
    disposals.push(store.dispose);
    const events = mode === "grace" ? store : f.transport;
    const refs = ["chat-a", "chat-b"].map(
      (agentId) => ({ kind: "transcript", agentId }) as const,
    );
    let acknowledgedGeneration = 0;
    const settle = async () => {
      await vi.advanceTimersByTimeAsync(50);
      const request = f.post.mock.calls.at(-1)?.[1];
      if (request && request.gen > acknowledgedGeneration) {
        acknowledgedGeneration = request.gen;
        f.source.emit("subscriptions", { gen: request.gen });
      }
      await vi.advanceTimersByTimeAsync(50);
    };
    let stop = events.watchResourceChanges(refs[0], () => {});
    await settle();
    stop();
    stop = events.watchResourceChanges(refs[1], () => {});
    await settle();
    const baseline = f.post.mock.calls.length;
    for (let step = 0; step < 10; step++) {
      stop();
      stop = events.watchResourceChanges(refs[step % refs.length], () => {});
      await settle();
    }
    expect(f.sources).toHaveLength(1);
    expect(f.source.closed).toBe(false);
    const posts = f.post.mock.calls.length - baseline;
    measurements.push({ mode, switches: 10, subscriptionPosts: posts });
    expect(posts).toBe(mode === "grace" ? 0 : 10);
    stop();
    store.dispose();
    f.transport.dispose();
  }
  console.info(
    "T28 subscription POST measurements",
    JSON.stringify(measurements),
  );
});

it("new-epoch entity fragments publish only after complete persisted staging and survive the next fragment without resetting", async () => {
  const f = await fixture();
  const listener = vi.fn();
  f.transport.watchResourceChanges({ kind: "state" }, listener);
  listener.mockClear();
  const encoded = JSON.stringify({
    after: 0,
    through: 2,
    documents: [
      {
        op: "upsert",
        collection: "workspace",
        id: "current",
        seq: 2,
        value: { stateDir: "fixture" },
      },
    ],
  });
  const frame = {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "new-epoch",
    revision: 2,
    reason: "initial",
    resource: { kind: "state" },
    cursor: { workspaceId: "workspace", seq: 2 },
    replyKind: "snapshot",
    transferId: "new-baseline",
    pageIndex: 0,
    partCount: 2,
  };
  f.source.emit("entity-transfer", {
    ...frame,
    partIndex: 0,
    complete: false,
    data: encoded.slice(0, 40),
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.staged).not.toHaveBeenCalled();
  expect(listener).not.toHaveBeenCalled();
  let finish!: () => void;
  f.staged.mockImplementation(
    () =>
      new Promise<void>((resolve) => {
        finish = resolve;
      }),
  );
  f.source.emit("entity-transfer", {
    ...frame,
    partIndex: 1,
    complete: true,
    data: encoded.slice(40),
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.staged).toHaveBeenCalledOnce();
  expect(listener).not.toHaveBeenCalled();
  finish();
  await vi.advanceTimersByTimeAsync(0);
  expect(listener).toHaveBeenCalledOnce();
  expect(listener).toHaveBeenCalledWith(
    expect.objectContaining({ cursor: { workspaceId: "workspace", seq: 2 } }),
  );
  expect(f.get).not.toHaveBeenCalled();
  expect(f.sources).toHaveLength(1);
});
it("a cancelled staging failure cannot close its replacement stream", async () => {
  const f = await fixture();
  let fail!: (error: Error) => void;
  f.staged.mockImplementation(
    () =>
      new Promise<void>((_resolve, reject) => {
        fail = reject;
      }),
  );
  f.source.emit("entity-transfer", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    reason: "initial",
    resource: { kind: "state" },
    cursor: { workspaceId: "workspace", seq: 2 },
    replyKind: "snapshot",
    transferId: "cancelled",
    pageIndex: 0,
    partIndex: 0,
    partCount: 1,
    complete: true,
    data: JSON.stringify({ after: 0, through: 2, documents: [] }),
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.staged).toHaveBeenCalledOnce();
  f.source.onerror?.();
  await vi.advanceTimersByTimeAsync(1000);
  expect(f.sources).toHaveLength(2);
  const replacement = f.sources[1]!;
  fail(new Error("old staging failed after cancellation"));
  await vi.advanceTimersByTimeAsync(0);
  expect(replacement.closed).toBe(false);
  expect(f.sources).toHaveLength(2);
});

it("reopens a terminal with a fresh snapshot after its reset base could not persist", async () => {
  const f = await fixture();
  const resource = { kind: "terminal-output", terminalId: "shell" } as const;
  const cursor = { workspaceId: "workspace", seq: 40 };
  f.readCursor.mockImplementation(async (ref) =>
    ref.kind === "terminal-output" ? cursor : undefined,
  );
  let stop = f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  const frame = (revision: number, text: string, offset: number) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision,
    reason: "change",
    resources: [resource],
    resourceVersions: [
      {
        revision,
        cursor: { workspaceId: "workspace", seq: offset },
        replyKind: "snapshot",
        viewSnapshot: {
          terminalOutput: { text, offset, status: "running", truncated: false },
        },
      },
    ],
  });
  f.source.emit("resources", {
    ...frame(10, "o".repeat(40), 40),
    reason: "initial",
  });
  await vi.advanceTimersByTimeAsync(0);
  const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
  f.persist.mockRejectedValueOnce(new Error("disk full"));
  f.source.emit("resources", frame(11, "new", 3));
  await vi.advanceTimersByTimeAsync(0);
  stop();
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 2 });
  stop = f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  expect(f.post).toHaveBeenLastCalledWith(
    "/api/sync/subscriptions",
    expect.objectContaining({ add: [{ resource }] }),
    expect.anything(),
  );
  f.source.emit("subscriptions", { gen: 3 });
  f.source.onerror?.();
  await vi.advanceTimersByTimeAsync(10000);
  const url = new URL(f.urls.at(-1)!, "http://studio.test");
  expect(
    JSON.parse(url.searchParams.get("since") ?? "[]").some(
      (entry: { resource: unknown }) =>
        JSON.stringify(entry.resource) === JSON.stringify(resource),
    ),
  ).toBe(false);
  const resumed = f.sources.at(-1)!;
  resumed.emit("session", { sessionId: "session-two" });
  resumed.emit("api-schema", { hash: API_SCHEMA_HASH });
  resumed.emit("resources", { ...frame(12, "new", 3), reason: "reconnect" });
  await vi.advanceTimersByTimeAsync(0);
  f.readCursor.mockImplementation(async (ref) =>
    ref.kind === "terminal-output"
      ? { workspaceId: "workspace", seq: 3 }
      : undefined,
  );
  resumed.onerror?.();
  await vi.advanceTimersByTimeAsync(10000);
  const recovered = new URL(f.urls.at(-1)!, "http://studio.test");
  expect(JSON.parse(recovered.searchParams.get("since")!)).toContainEqual({
    resource,
    since: { workspaceId: "workspace", seq: 3 },
  });
  stop();
  quiet.mockRestore();
});

it("takes terminal ownership with a snapshot and resumes only its proven baseline", async () => {
  const f = await fixture(true);
  const resource = {
    kind: "terminal-output",
    terminalId: "taken-over",
  } as const;
  f.readCursor.mockImplementation(async (ref) =>
    ref.kind === "terminal-output"
      ? { workspaceId: "workspace", seq: 40 }
      : undefined,
  );
  f.transport.watchResourceChanges(resource, vi.fn());
  const frame = (revision: number, text: string, offset: number) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision,
    reason: "change",
    resources: [resource],
    resourceVersions: [
      {
        revision,
        cursor: { workspaceId: "workspace", seq: offset },
        replyKind: "snapshot",
        viewSnapshot: {
          terminalOutput: { text, offset, status: "running", truncated: false },
        },
      },
    ],
  });
  f.source.emit("resources", {
    ...frame(10, "o".repeat(40), 40),
    reason: "initial",
  });
  await vi.advanceTimersByTimeAsync(0);
  const reset = frame(11, "new", 3);
  // This is what an owner's failed snapshot dispatch broadcasts to a follower.
  const {
    cursor: _cursor,
    replyKind: _kind,
    ...revoked
  } = reset.resourceVersions[0];
  f.source.emit("resources", { ...reset, resourceVersions: [revoked] });
  await vi.advanceTimersByTimeAsync(0);
  f.takeOwnership();
  await vi.advanceTimersByTimeAsync(50);
  const coldUrl = new URL(f.urls.at(-1)!, "http://studio.test");
  expect(
    JSON.parse(coldUrl.searchParams.get("since") ?? "[]").some(
      (entry: { resource: unknown }) =>
        JSON.stringify(entry.resource) === JSON.stringify(resource),
    ),
  ).toBe(false);
  const owned = f.sources.at(-1)!;
  owned.emit("session", { sessionId: "owned" });
  owned.emit("api-schema", { hash: API_SCHEMA_HASH });
  owned.emit("resources", { ...frame(12, "new", 3), reason: "initial" });
  await vi.advanceTimersByTimeAsync(0);
  f.readCursor.mockImplementation(async (ref) =>
    ref.kind === "terminal-output"
      ? { workspaceId: "workspace", seq: 3 }
      : undefined,
  );
  owned.onerror?.();
  await vi.advanceTimersByTimeAsync(10000);
  const resumed = new URL(f.urls.at(-1)!, "http://studio.test");
  expect(JSON.parse(resumed.searchParams.get("since")!)).toContainEqual({
    resource,
    since: { workspaceId: "workspace", seq: 3 },
  });
});

it("a late terminal follower consumes owner merged views without repeated recovery GETs", async () => {
  const owner = await fixture(false, true, true);
  const { mergeTerminalOutput } =
    await import("./subscriptions/terminalOutput");
  const { createDataObserver } = await import("./subscriptions/data");
  const resource = {
    kind: "terminal-output",
    terminalId: "late-follower",
  } as const;
  owner.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  owner.source.emit("subscriptions", { gen: 1 });
  let merged: Parameters<typeof mergeTerminalOutput>[0];
  owner.persist.mockImplementation(async (event, current) => {
    if (!current()) return;
    for (const entry of event.resourceVersions) {
      if (!entry.viewSnapshot?.terminalOutput) continue;
      merged = mergeTerminalOutput(
        merged,
        entry.viewSnapshot.terminalOutput,
        entry.replyKind === "delta",
      );
      entry.viewSnapshot = { terminalOutput: merged };
    }
  });
  const frame = (
    revision: number,
    text: string,
    offset: number,
    replyKind = "delta",
  ) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision,
    reason: "change",
    resources: [resource],
    resourceVersions: [
      {
        revision,
        cursor: { workspaceId: "workspace", seq: offset },
        replyKind,
        viewSnapshot: {
          terminalOutput: { text, offset, status: "running", truncated: false },
        },
      },
    ],
  });
  owner.source.emit("resources", {
    ...frame(10, "old", 3, "snapshot"),
    reason: "initial",
  });
  await vi.advanceTimersByTimeAsync(0);
  const follower = await fixture(true);
  const recover = vi.fn(async () => merged);
  const accept = vi.fn();
  const stop = createDataObserver(
    () => ({}) as any,
    follower.transport,
    undefined,
    recover,
  )(resource, accept, vi.fn());
  const relay = () => {
    const message = owner.posted
      .filter((message) => message.kind === "resource-event")
      .at(-1)!;
    follower.source.emit("resources", message.event);
    return message.event;
  };
  owner.peerMessage({
    kind: "subscriptions",
    resources: [resource],
    reset: true,
    tokenRates: false,
  });
  const replay = relay();
  await vi.advanceTimersByTimeAsync(0);
  expect(replay.resourceVersions[0]).toMatchObject({
    replyKind: "snapshot",
    viewSnapshot: { terminalOutput: { text: "old" } },
  });
  expect(accept).toHaveBeenLastCalledWith(
    merged,
    expect.objectContaining({
      epoch: "epoch",
      revision: 10,
      replyKind: "snapshot",
      cursor: { workspaceId: "workspace", seq: 3 },
    }),
  );
  for (const revision of [11, 12]) {
    owner.source.emit("resources", frame(revision, "!", revision - 7));
    await vi.advanceTimersByTimeAsync(0);
    const broadcast = relay();
    expect(broadcast.resourceVersions[0]).toMatchObject({
      replyKind: "snapshot",
      viewSnapshot: { terminalOutput: merged },
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(accept).toHaveBeenLastCalledWith(
      merged,
      expect.objectContaining({
        epoch: "epoch",
        revision,
        replyKind: "snapshot",
        cursor: { workspaceId: "workspace", seq: revision - 7 },
      }),
    );
  }
  let finish!: () => void;
  const apply = owner.persist.getMockImplementation()!;
  owner.persist.mockImplementationOnce(async (event, current) => {
    await new Promise<void>((resolve) => {
      finish = resolve;
    });
    await apply(event, current);
  });
  const before = owner.posted.filter(
    (message) => message.kind === "resource-event",
  ).length;
  owner.source.emit("resources", frame(13, "!", 6));
  await vi.advanceTimersByTimeAsync(0);
  expect(
    owner.posted.filter((message) => message.kind === "resource-event"),
  ).toHaveLength(before);
  finish();
  await vi.advanceTimersByTimeAsync(0);
  expect(relay().resourceVersions[0]).toMatchObject({
    replyKind: "snapshot",
    viewSnapshot: { terminalOutput: { text: "old!!!" } },
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(recover).not.toHaveBeenCalled();
  const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
  const beforeFailure = owner.posted.filter(
    (message) => message.kind === "resource-event",
  ).length;
  owner.persist.mockRejectedValueOnce(new Error("disk full"));
  owner.source.emit("resources", frame(14, "new", 3, "snapshot"));
  await vi.advanceTimersByTimeAsync(0);
  expect(
    owner.posted.filter((message) => message.kind === "resource-event"),
  ).toHaveLength(beforeFailure);
  expect(owner.source.closed).toBe(true);
  await vi.advanceTimersByTimeAsync(1000);
  expect(owner.sources).toHaveLength(2);
  const retry = new URL(owner.urls.at(-1)!, "http://studio.test");
  expect(
    JSON.parse(retry.searchParams.get("since") ?? "[]").some(
      (entry: { resource: unknown }) =>
        JSON.stringify(entry.resource) === JSON.stringify(resource),
    ),
  ).toBe(false);
  quiet.mockRestore();
  stop();
});

it.each([false, true])(
  "a follower scoped application failure requests owner recovery only while subscribed (cancelled=%s)",
  async (cancelled) => {
    const f = await fixture(true);
    const ref = { kind: "work", agentId: "lead" } as const;
    const stop = f.transport.watchResourceChanges(ref, () => {});
    let reject!: (error: Error) => void;
    f.persist.mockReturnValueOnce(
      new Promise((_resolve, failed) => {
        reject = failed;
      }),
    );
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    f.source.emit("resources", {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision: 2,
      reason: "initial",
      resources: [ref],
      resourceVersions: [
        {
          resource: ref,
          revision: 2,
          cursor: { workspaceId: "workspace", seq: 2 },
          replyKind: "snapshot",
          entityChanges: { after: 0, through: 2, documents: [] },
        },
      ],
    });
    await vi.advanceTimersByTimeAsync(0);
    if (cancelled) stop();
    reject(new Error("local follower write unavailable"));
    await vi.advanceTimersByTimeAsync(0);
    const requests = f.messages.filter(
      (message) => (message as { kind: string }).kind === "resource-recovery",
    );
    expect(requests).toHaveLength(cancelled ? 0 : 1);
    if (!cancelled) expect(requests[0]).toMatchObject({ resources: [ref] });
    expect(f.get).not.toHaveBeenCalled();
    error.mockRestore();
  },
);

it("the owner coalesces a subscribed peer recovery request into its bounded cursor reconnect", async () => {
  const f = await fixture(false, true, true);
  const ref = { kind: "state" };
  f.control("subscriptions", {
    resources: [ref],
    tokenRates: false,
    reset: false,
  });
  f.control("resource-recovery", { resources: [ref] });
  expect(f.source.closed).toBe(true);
  f.control("resource-recovery", { resources: [ref] });
  await vi.advanceTimersByTimeAsync(1000);
  expect(f.sources).toHaveLength(2);
  expect(f.get).not.toHaveBeenCalled();
});

it("a failed Work write fences its queued terminal reset before reconnect cursors are read", async () => {
  const f = await fixture();
  const terminal = {
    kind: "terminal-output",
    terminalId: "queued-reset",
  } as const;
  const work = { kind: "work", agentId: "lead" } as const;
  f.readCursor.mockImplementation(async (ref) =>
    ref.kind === "terminal-output"
      ? { workspaceId: "workspace", seq: 40 }
      : undefined,
  );
  f.transport.watchResourceChanges(terminal, vi.fn());
  f.transport.watchResourceChanges(work, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  const snapshot = (revision: number, text: string, offset: number) => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision,
    reason: "change",
    resources: [terminal],
    resourceVersions: [
      {
        revision,
        cursor: { workspaceId: "workspace", seq: offset },
        replyKind: "snapshot",
        viewSnapshot: {
          terminalOutput: { text, offset, status: "running", truncated: false },
        },
      },
    ],
  });
  f.source.emit("resources", {
    ...snapshot(10, "o".repeat(40), 40),
    reason: "initial",
  });
  await vi.advanceTimersByTimeAsync(0);
  let reject!: (error: Error) => void;
  f.persist.mockReturnValueOnce(
    new Promise<void>((_resolve, failed) => {
      reject = failed;
    }),
  );
  const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
  f.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 11,
    reason: "change",
    resources: [work],
    resourceVersions: [
      {
        revision: 11,
        resource: work,
        cursor: { workspaceId: "workspace", seq: 5 },
        replyKind: "snapshot",
        entityChanges: { after: 0, through: 5, documents: [] },
      },
    ],
  });
  await vi.advanceTimersByTimeAsync(0);
  f.source.emit("resources", snapshot(12, "new", 3));
  reject(new Error("Work storage unavailable"));
  await vi.advanceTimersByTimeAsync(0);
  expect(f.source.closed).toBe(true);
  await vi.advanceTimersByTimeAsync(1000);
  expect(f.sources).toHaveLength(2);
  const resumed = new URL(f.urls.at(-1)!, "http://studio.test");
  expect(
    JSON.parse(resumed.searchParams.get("since") ?? "[]").some(
      (entry: { resource: unknown }) =>
        JSON.stringify(entry.resource) === JSON.stringify(terminal),
    ),
  ).toBe(false);
  quiet.mockRestore();
});

it("relays legacy revision-only hints from an owner to a follower", async () => {
  const owner = await fixture(false, true, true);
  const follower = await fixture(true);
  const accept = vi.fn();
  follower.transport.watchResourceChanges({ kind: "state" }, accept);
  accept.mockClear();
  owner.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 20,
    reason: "change",
    resources: [{ kind: "state" }],
  });
  await vi.advanceTimersByTimeAsync(0);
  const message = owner.posted
    .filter((message) => message.kind === "resource-event")
    .at(-1)!;
  expect(message.event.revision).toBe(20);
  expect(message.event.resourceVersions).toBeUndefined();
  follower.source.emit("resources", message.event);
  await vi.advanceTimersByTimeAsync(0);
  expect(accept).toHaveBeenCalledOnce();
});

function health(
  status = "live",
  generation = 1,
  sequence = 1,
  validatedAt = Date.now(),
) {
  return { status, generation, sequence, sentAt: Date.now(), validatedAt };
}

it("connection: fresh healthy owner proof recovers a quiet follower without reads", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  await vi.advanceTimersByTimeAsync(12_000);
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  const reads = f.get.mock.calls.length;
  for (let sequence = 1; sequence <= 20; sequence++) {
    f.control("leader-heartbeat", { health: health("live", 1, sequence) });
    expect(status.mock.lastCall?.[0]).toBe("live");
    await vi.advanceTimersByTimeAsync(3_000);
  }
  expect(f.sources).toHaveLength(0);
  expect(f.get).toHaveBeenCalledTimes(reads);
});

it("connection: peer liveness and degraded owner proof never heal a follower", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  await vi.advanceTimersByTimeAsync(12_000);
  for (let sequence = 1; sequence <= 20; sequence++) {
    f.control("leader-heartbeat", {});
    f.control("leader-heartbeat", { health: health("degraded", 1, sequence) });
    await vi.advanceTimersByTimeAsync(3_000);
    expect(status.mock.lastCall?.[0]).toBe("degraded");
  }
});

it("connection: rejects replayed, expired, wrong workspace/schema and replaced-source health", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  const prior = health("live", 1, 1);
  f.control("leader-heartbeat", { health: prior });
  f.control("status", { status: "degraded", health: health("degraded", 2, 2) });
  for (const fields of [
    { health: prior },
    { health: health("live", 1, 3) },
    { health: health("live", 2, 2) },
    { health: health("live", 2, 3, Date.now() - 45_001) },
    { workspaceId: "elsewhere", health: health("live", 2, 4) },
    { apiSchemaHash: "old-schema", health: health("live", 2, 5) },
  ]) {
    f.control("leader-heartbeat", fields);
    expect(status.mock.lastCall?.[0]).toBe("degraded");
  }
  f.control("leader-heartbeat", { health: health("live", 2, 6) });
  expect(status.mock.lastCall?.[0]).toBe("live");
});

it("connection: live status without current server proof cannot heal", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  await vi.advanceTimersByTimeAsync(12_000);
  f.control("status", { status: "live" });
  expect(status.mock.lastCall?.[0]).toBe("degraded");
});

it("connection: browser offline hint preserves a validated stream and online is not proof", async () => {
  const f = await fixture();
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  Object.assign(navigator, { onLine: false });
  window.dispatchEvent(new Event("offline"));
  expect(f.source.closed).toBe(false);
  expect(status.mock.lastCall?.[0]).toBe("live");
  f.source.onerror?.();
  expect(status.mock.lastCall?.[0]).toBe("offline");
  Object.assign(navigator, { onLine: true });
  window.dispatchEvent(new Event("online"));
  await vi.advanceTimersByTimeAsync(60);
  expect(f.sources).toHaveLength(2);
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  f.sources[1]!.onopen?.();
  f.sources[1]!.emit("api-schema", { hash: API_SCHEMA_HASH });
  expect(status.mock.lastCall?.[0]).toBe("degraded");
});

it("connection: a slow cursor read cannot be starved by owner peer heartbeats during recovery", async () => {
  const random = vi.spyOn(Math, "random").mockReturnValue(0.5);
  disposals.push(() => random.mockRestore());
  const f = await fixture(false, true, true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.readCursor.mockImplementation(
    () => new Promise((resolve) => setTimeout(() => resolve(undefined), 4_000)),
  );
  f.readCursor.mockClear();
  f.source.onerror?.();
  expect(f.source.closed).toBe(true);
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  await vi.advanceTimersByTimeAsync(30_000);
  // The server is available for every open attempt. Only local cursor reads
  // delay this path; peer liveness must not cancel their result every 3 seconds.
  expect(f.sources).toHaveLength(2);
  expect(f.readCursor).toHaveBeenCalledOnce();
  const replacement = f.sources[1]!;
  replacement.emit("api-schema", { hash: API_SCHEMA_HASH });
  replacement.emit("heartbeat", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 1,
  });
  expect(status.mock.lastCall?.[0]).toBe("live");
});

it("connection: an unfinished cursor read opens a full baseline and cannot later replace its stream", async () => {
  const f = await fixture(false, true, true);
  let finish!: (cursor: { workspaceId: string; seq: number }) => void;
  f.readCursor.mockImplementation(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.source.onerror?.();
  await vi.advanceTimersByTimeAsync(10_000);
  expect(f.sources).toHaveLength(2);
  expect(f.readCursor).toHaveBeenCalledTimes(2);
  const replacement = f.sources[1]!;
  expect(new URL(f.urls[1]!, location.origin).searchParams.get("since")).toBe(
    "[]",
  );
  replacement.emit("api-schema", { hash: API_SCHEMA_HASH });
  replacement.emit("heartbeat", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 1,
  });
  finish({ workspaceId: "workspace", seq: 99 });
  await vi.advanceTimersByTimeAsync(0);
  expect(f.sources).toHaveLength(2);
  expect(replacement.closed).toBe(false);
  expect(status.mock.lastCall?.[0]).toBe("live");
});

it("connection: switching Studio screens during cursor preparation opens the latest resource union", async () => {
  const f = await fixture();
  f.readCursor.mockImplementation(
    () => new Promise((resolve) => setTimeout(() => resolve(undefined), 4_000)),
  );
  f.readCursor.mockClear();
  f.source.onerror?.();
  await vi.advanceTimersByTimeAsync(1_000);
  const firstChat = { kind: "queue", agentId: "first-chat" } as const;
  const secondChat = { kind: "queue", agentId: "second-chat" } as const;
  const stopFirst = f.transport.watchResourceChanges(firstChat, vi.fn());
  await vi.advanceTimersByTimeAsync(1_000);
  stopFirst();
  f.transport.watchResourceChanges(secondChat, vi.fn());
  await vi.advanceTimersByTimeAsync(3_000);
  expect(f.sources).toHaveLength(2);
  const requested = JSON.parse(
    new URL(f.urls[1]!, location.origin).searchParams.get("resources")!,
  );
  expect(requested).toContainEqual({ kind: "state" });
  expect(requested).toContainEqual(secondChat);
  expect(requested).not.toContainEqual(firstChat);
  expect(f.readCursor).toHaveBeenCalledOnce();
});

it.each(["hide", "dispose", "last-subscriber"] as const)(
  "connection: %s cancels cursor preparation and late completion cannot reopen",
  async (action) => {
    const f = await fixture(false, true, true);
    let finish!: () => void;
    f.readCursor.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finish = () => resolve(undefined);
        }),
    );
    f.source.onerror?.();
    await vi.advanceTimersByTimeAsync(1_000);
    const status = vi.fn();
    f.transport.watchResourceConnection(status);
    expect(status.mock.lastCall?.[1]).toBe("preparing");
    if (action === "hide") {
      Object.assign(document, { hidden: true });
      document.dispatchEvent(new Event("visibilitychange"));
    } else if (action === "dispose") f.transport.dispose();
    else f.stopState();
    await vi.advanceTimersByTimeAsync(2_000);
    finish();
    await vi.advanceTimersByTimeAsync(RESOURCE_CURSOR_PREPARATION_TIMEOUT_MS);
    expect(f.sources).toHaveLength(1);
    if (action === "hide") {
      Object.assign(document, { hidden: false });
      document.dispatchEvent(new Event("visibilitychange"));
      await vi.advanceTimersByTimeAsync(60);
      expect(f.sources).toHaveLength(2);
      expect(f.sources[1]!.closed).toBe(false);
    }
  },
);

it("connection: a subscription diff uses a full baseline after bounded cursor preparation", async () => {
  const f = await fixture();
  let finish!: () => void;
  f.readCursor.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = () => resolve(undefined);
      }),
  );
  const chat = { kind: "queue", agentId: "slow-diff" } as const;
  f.transport.watchResourceChanges(chat, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  expect(f.post).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(RESOURCE_CURSOR_PREPARATION_TIMEOUT_MS);
  expect(f.post).toHaveBeenCalledOnce();
  expect(f.post.mock.calls[0]![1]).toMatchObject({
    gen: 1,
    add: [{ resource: chat }],
    remove: [],
  });
  f.source.emit("subscriptions", { gen: 1 });
  finish();
  await vi.advanceTimersByTimeAsync(0);
  expect(f.post).toHaveBeenCalledOnce();
  expect(f.sources).toHaveLength(1);
});

it("connection: retry activity and deadline describe actual work and clear only on validated recovery", async () => {
  const random = vi.spyOn(Math, "random").mockReturnValue(0.5);
  disposals.push(() => random.mockRestore());
  const f = await fixture();
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.source.onopen?.();
  expect(status.mock.lastCall?.slice(0, 2)).toEqual(["live", "open"]);
  let finish!: () => void;
  f.readCursor.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = () => resolve(undefined);
      }),
  );
  const failedAt = Date.now();
  f.source.onerror?.();
  expect(status.mock.lastCall).toEqual([
    "degraded",
    "retry-scheduled",
    { failure: "stream-error", retryAt: failedAt + 500 },
  ]);
  await vi.advanceTimersByTimeAsync(500);
  expect(status.mock.lastCall).toEqual([
    "degraded",
    "preparing",
    { failure: "stream-error", retryAt: undefined },
  ]);
  finish();
  await vi.advanceTimersByTimeAsync(0);
  expect(status.mock.lastCall?.slice(0, 2)).toEqual(["degraded", "connecting"]);
  f.sources[1]!.onopen?.();
  f.sources[1]!.emit("api-schema", { hash: API_SCHEMA_HASH });
  expect(status.mock.lastCall?.[2].failure).toBe("stream-error");
  f.sources[1]!.emit("heartbeat", {
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 1,
  });
  expect(status.mock.lastCall).toEqual([
    "live",
    "open",
    { failure: undefined, retryAt: undefined },
  ]);
  f.sources[1]!.onerror?.();
  expect(status.mock.lastCall?.[2].retryAt).toBeGreaterThan(Date.now());
  Object.assign(document, { hidden: true });
  document.dispatchEvent(new Event("visibilitychange"));
  expect(status.mock.lastCall).toEqual([
    "paused",
    "paused",
    { failure: "stream-error", retryAt: undefined },
  ]);
});

it("connection: browser reachability activity changes without discarding validated stream health", async () => {
  const f = await fixture();
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.source.onopen?.();
  Object.assign(navigator, { onLine: false });
  window.dispatchEvent(new Event("offline"));
  expect(status.mock.lastCall?.slice(0, 2)).toEqual(["live", "offline"]);
  expect(f.source.closed).toBe(false);
  Object.assign(navigator, { onLine: true });
  window.dispatchEvent(new Event("online"));
  await vi.advanceTimersByTimeAsync(60);
  expect(status.mock.lastCall?.slice(0, 2)).toEqual(["live", "open"]);
  expect(f.sources).toHaveLength(1);
});

it("connection: unchanged server heartbeats do not repeat live observer side effects", async () => {
  const f = await fixture();
  f.source.onopen?.();
  const retryPendingWrites = vi.fn();
  f.transport.watchResourceConnection((status) => {
    if (status === "live") retryPendingWrites();
  });
  expect(retryPendingWrites).toHaveBeenCalledOnce();
  for (let revision = 2; revision <= 5; revision++) {
    f.source.emit("heartbeat", {
      workspaceId: "workspace",
      epoch: "epoch",
      revision,
    });
  }
  expect(retryPendingWrites).toHaveBeenCalledOnce();
});

it("connection: hidden pause is local and real resume events coalesce once", async () => {
  const f = await fixture();
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  Object.assign(document, { hidden: true });
  document.dispatchEvent(new Event("visibilitychange"));
  expect(status.mock.lastCall?.[0]).toBe("paused");
  expect(f.source.closed).toBe(true);
  Object.assign(document, { hidden: false });
  document.dispatchEvent(new Event("visibilitychange"));
  window.dispatchEvent(new Event("online"));
  window.dispatchEvent(new Event("pageshow"));
  await vi.advanceTimersByTimeAsync(100);
  expect(f.sources).toHaveLength(2);
  expect(status.mock.lastCall?.[0]).toBe("connecting");
});

it("connection: hidden follower ignores live peer frames and visible follower recovers from paused owner", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  Object.assign(document, { hidden: true });
  document.dispatchEvent(new Event("visibilitychange"));
  f.control("leader-heartbeat", { health: health() });
  expect(status.mock.lastCall?.[0]).toBe("paused");
  Object.assign(document, { hidden: false });
  document.dispatchEvent(new Event("visibilitychange"));
  await vi.advanceTimersByTimeAsync(60);
  f.control("status", { status: "paused", health: health("paused", 1, 2) });
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  f.control("leader-heartbeat", { health: health("live", 2, 3) });
  expect(status.mock.lastCall?.[0]).toBe("live");
});

it("connection: real lifecycle removes every registered listener across disposal and restart", async () => {
  const f = await fixture();
  for (let cycle = 0; cycle < 2; cycle++) {
    f.transport.dispose();
    for (const target of [window, document]) {
      const added = vi.mocked(target.addEventListener).mock.calls;
      const removed = vi.mocked(target.removeEventListener).mock.calls;
      expect(
        added.every(([kind, fn]) =>
          removed.some(([name, listener]) => name === kind && listener === fn),
        ),
      ).toBe(true);
    }
    if (cycle === 0) {
      f.transport.watchResourceChanges({ kind: "state" }, vi.fn());
      await vi.advanceTimersByTimeAsync(0);
    }
  }
});

it("connection: owner health snapshots retain server freshness and invalidate replacement source", async () => {
  const f = await fixture(false, true, true);
  await vi.advanceTimersByTimeAsync(3_000);
  const first = [...f.posted]
    .reverse()
    .find((message) => message.kind === "leader-heartbeat");
  expect(first.health).toMatchObject({
    status: "live",
    validatedAt: expect.any(Number),
  });
  await vi.advanceTimersByTimeAsync(3_000);
  const quiet = [...f.posted]
    .reverse()
    .find((message) => message.kind === "leader-heartbeat");
  expect(quiet.health.validatedAt).toBe(first.health.validatedAt);
  expect(quiet.health.sequence).toBeGreaterThan(first.health.sequence);
  f.source.onerror?.();
  const failed = [...f.posted]
    .reverse()
    .find((message) => message.kind === "status");
  expect(failed.health.status).toBe("degraded");
  expect(failed.health.validatedAt).toBeUndefined();
  expect(failed.health.generation).toBeGreaterThan(first.health.generation);
});

it("connection: first and late quiet followers recover only from fresh scoped owner proof", async () => {
  const f = await fixture(true, true, false, false);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  expect(status.mock.lastCall?.[0]).toBe("connecting");
  f.control("leader-heartbeat", {});
  f.control("status", { status: "live" });
  expect(status.mock.lastCall?.[0]).toBe("connecting");
  f.control("leader-heartbeat", { health: health() });
  expect(status.mock.lastCall?.[0]).toBe("live");
});

it("connection: offline hint preserves fresh follower proof and plain heartbeats cannot extend it", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  Object.assign(navigator, { onLine: false });
  window.dispatchEvent(new Event("offline"));
  expect(status.mock.lastCall?.[0]).toBe("live");
  Object.assign(navigator, { onLine: true });
  for (let i = 0; i < 5; i++) {
    f.control("leader-heartbeat", {});
    await vi.advanceTimersByTimeAsync(3_000);
  }
  expect(status.mock.lastCall?.[0]).toBe("degraded");
});

it("connection: owner A B A can recover while prior ownership replay stays rejected", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.control("leader-heartbeat", { health: health("live", 1, 1) });
  await vi.advanceTimersByTimeAsync(1);
  f.control("leader-heartbeat", {
    tabId: "owner-B",
    health: health("degraded", 1, 1),
  });
  f.control("leader-heartbeat", { health: health("live", 1, 2) });
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  f.control("leader-heartbeat", { health: health("live", 2, 3) });
  expect(status.mock.lastCall?.[0]).toBe("live");
});

it("connection: degraded owner cached resource and token replay applies without healing", async () => {
  const f = await fixture(true);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.control("status", { status: "degraded", health: health("degraded", 2, 1) });
  f.control("resource-event", {
    health: health("degraded", 2, 2),
    event: {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision: 1,
      reason: "initial",
      resources: [{ kind: "state" }],
      resourceVersions: [{ revision: 1 }],
    },
  });
  await vi.advanceTimersByTimeAsync(0);
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  f.control("token-rates", {
    health: health("degraded", 2, 3),
    event: {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision: 2,
      snapshot: true,
      removed: [],
      records: [],
    },
  });
  expect(status.mock.lastCall?.[0]).toBe("degraded");
});

it("connection: cancelling a pending server frame cannot publish live proof for replacement", async () => {
  const f = await fixture(false, true, true);
  let finish!: () => void;
  f.persist.mockImplementationOnce(
    () =>
      new Promise<void>((resolve) => {
        finish = resolve;
      }),
  );
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.source.emit("resources", {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 2,
    reason: "change",
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: 2,
        cursor: { workspaceId: "workspace", seq: 2 },
        replyKind: "delta",
      },
    ],
  });
  await vi.advanceTimersByTimeAsync(0);
  f.source.onerror?.();
  finish();
  await vi.advanceTimersByTimeAsync(1_000);
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  const failed = [...f.posted]
    .reverse()
    .find((message) => message.kind === "status");
  expect(failed.health.validatedAt).toBeUndefined();
});

it("connection: actual lock and channel share the version and schema coordination scope", async () => {
  const f = await fixture(false, true, true);
  const names = resourceCoordinationNames(location.origin, "workspace");
  expect(f.lockNames).toEqual([names.lock]);
  expect(f.channelNames).toEqual([names.channel]);
  for (const name of Object.values(names)) {
    expect(name).toContain(
      `${RESOURCE_COORDINATION_VERSION}:${API_SCHEMA_HASH}:${location.origin}:workspace`,
    );
  }
  expect(resourceCoordinationNames(location.origin, "other")).not.toEqual(
    names,
  );
  expect(
    resourceCoordinationNames("http://other.test", "workspace"),
  ).not.toEqual(names);
});

it("connection: validated progress keeps owner health fresh through a false offline hint", async () => {
  const f = await fixture(false, true, true);
  const resource = { kind: "vault-readiness", prefix: "vault" } as const;
  f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  const follower = await fixture(true);
  follower.transport.watchResourceChanges(resource, vi.fn());
  const followerStatus = vi.fn();
  follower.transport.watchResourceConnection(followerStatus);
  for (let revision = 2; revision <= 4; revision++) {
    await vi.advanceTimersByTimeAsync(20_000);
    f.source.emit("resource-progress", {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "workspace",
      epoch: "epoch",
      revision,
      resource,
      value: true,
    });
    const relay = [...f.posted]
      .reverse()
      .find((message) => message.kind === "resource-progress");
    follower.control("resource-progress", relay);
    expect(followerStatus.mock.lastCall?.[0]).toBe("live");
  }
  Object.assign(navigator, { onLine: false });
  window.dispatchEvent(new Event("offline"));
  expect(f.source.closed).toBe(false);
  expect(status.mock.lastCall?.[0]).toBe("live");
  const proof = [...f.posted]
    .reverse()
    .find((message) => message.kind === "resource-progress");
  expect(proof.health.validatedAt).toBe(Date.now());
});

it("connection: foreign, stale and invalid progress cannot heal a replacement stream", async () => {
  const f = await fixture(false, true, true);
  const resource = { kind: "vault-readiness", prefix: "vault" } as const;
  f.transport.watchResourceChanges(resource, vi.fn());
  await vi.advanceTimersByTimeAsync(50);
  f.source.emit("subscriptions", { gen: 1 });
  const progress = {
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId: "workspace",
    epoch: "epoch",
    revision: 10,
    resource,
    value: true,
  };
  f.source.emit("resource-progress", progress);
  const status = vi.fn();
  f.transport.watchResourceConnection(status);
  f.source.onerror?.();
  await vi.advanceTimersByTimeAsync(1_000);
  const replacement = f.sources[1]!;
  replacement.emit("api-schema", { hash: API_SCHEMA_HASH });
  for (const fields of [
    { workspaceId: "foreign" },
    { epoch: "foreign" },
    { revision: 9 },
  ]) {
    replacement.emit("resource-progress", { ...progress, ...fields });
    expect(status.mock.lastCall?.[0]).toBe("degraded");
  }
  const quiet = vi.spyOn(console, "error").mockImplementation(() => {});
  replacement.emit("resource-progress", { ...progress, value: "invalid" });
  expect(replacement.closed).toBe(true);
  expect(status.mock.lastCall?.[0]).toBe("degraded");
  quiet.mockRestore();
});
