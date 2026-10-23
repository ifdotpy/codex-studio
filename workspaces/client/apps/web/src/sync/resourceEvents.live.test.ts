import { afterEach, expect, it, vi } from "vitest";
import { createResourceEvents } from "./resourceEvents";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";

vi.mock("./client", () => ({ syncDatabase: vi.fn() }));
vi.mock("../usage/tokenRate", () => ({
  configureTokenRateStream: vi.fn(),
  receiveResourceTokenRates: vi.fn(),
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

async function fixture() {
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
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("location", { origin: "http://studio.test" });
  vi.stubGlobal("BroadcastChannel", undefined);
  const sources: Source[] = [];
  const urls: string[] = [];
  const post = vi.fn().mockResolvedValue({ gen: 1 });
  const persist = vi.fn().mockResolvedValue(undefined);
  const transport = createResourceEvents({
    api: {
      get: vi.fn(),
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
    },
    tokenRates: {
      configureTokenRateStream: vi.fn(),
      receiveResourceTokenRates: vi.fn(() => true),
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
  const source = sources[0]!;
  source.emit("session", { sessionId: "session-one" });
  source.emit("api-schema", { hash: API_SCHEMA_HASH });
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
  return { transport, source, sources, urls, post, persist, stopState };
}

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
    add: [{ kind: "transcript", agentId: "chat-final" }],
    remove: [],
  });
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
