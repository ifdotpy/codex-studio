import { afterEach, expect, it, vi } from "vitest";
import { createResourceEvents } from "./resourceEvents";
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
vi.mock("./resume", () => ({ onResume: () => () => {} }));

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

async function fixture(follower = false, session = true) {
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
    ...(follower
      ? {
          locks: {
            request: (
              _name: string,
              _options: unknown,
              callback: (lock: null) => Promise<void>,
            ) => callback(null),
          },
        }
      : {}),
  });
  vi.stubGlobal("location", { origin: "http://studio.test" });
  let deliverPeer: ((event: MessageEvent) => void) | undefined;
  vi.stubGlobal(
    "BroadcastChannel",
    follower
      ? class {
          onmessage: ((event: MessageEvent) => void) | null = null;
          constructor() {
            deliverPeer = (event) => this.onmessage?.(event);
          }
          postMessage() {}
          close() {}
        }
      : undefined,
  );
  const peer = (name: string, event: unknown) =>
    deliverPeer?.({
      data: {
        workspaceId: "workspace",
        tabId: "owner",
        apiSchemaHash: API_SCHEMA_HASH,
        kind: name === "token-rates" ? name : "resource-event",
        event,
      },
    } as MessageEvent);
  const sources: Source[] = [];
  const urls: string[] = [];
  const post = vi.fn().mockResolvedValue({ gen: 1 });
  const get = vi.fn().mockResolvedValue({});
  const readCursor = vi.fn().mockResolvedValue(undefined);
  const persist = vi.fn().mockResolvedValue(undefined);
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
    source,
    sources,
    urls,
    post,
    persist,
    stopState,
    get,
    readCursor,
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
