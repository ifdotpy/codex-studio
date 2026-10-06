// Round-two review harness. Real sync/resourceEvents.ts and api.ts. The stream is
// a model of scripts/studio_api/sync/resources/hub.py at head: every opened
// EventSource receives an "initial" baseline with per-resource revisions; publish_many
// stores the hub counter; publish_entity_sequence stores the entity sequence for state.
import { afterEach, describe, expect, it, vi } from "vitest";
import { API_SCHEMA_HASH } from "./generated/apiSchema";

const workspaceId = "b".repeat(32);
const { resumeListeners, syncDatabase } = vi.hoisted(() => ({
  resumeListeners: new Set<() => void>(),
  syncDatabase: vi.fn(),
}));
vi.mock("./sync/client", () => ({ syncDatabase }));
vi.mock("./usage/tokenRate", () => ({
  configureTokenRateStream: vi.fn(),
  receiveResourceTokenRates: vi.fn(),
}));
vi.mock("./sync/resume", () => ({
  onResume: (listener: () => void) => {
    resumeListeners.add(listener);
    return () => resumeListeners.delete(listener);
  },
}));

type Ref = { kind: string; [key: string]: unknown };
type HubEvent = {
  protocol: 3;
  workspaceId: string;
  epoch: string;
  revision: number;
  reason: string;
  resources: Ref[];
  resourceVersions: Array<{
    resource: Ref;
    revision: number;
    entitySequences?: number[];
  }>;
};
const keyOf = (ref: Ref) =>
  JSON.stringify(Object.fromEntries(Object.entries(ref).sort()));

class Hub {
  epoch = "epoch-1";
  revision = 0;
  revisions = new Map<string, number>();
  constructor(entitySequence = 0) {
    if (entitySequence)
      this.revisions.set(keyOf({ kind: "state" }), entitySequence);
  }
  event(reason: string, resources: Ref[]): HubEvent {
    return {
      protocol: 3,
      workspaceId,
      epoch: this.epoch,
      revision: this.revision,
      reason,
      resources,
      resourceVersions: resources.map((resource) => ({
        resource,
        revision: this.revisions.get(keyOf(resource)) ?? 0,
      })),
    };
  }
  publishMany(resources: Ref[]) {
    this.revision++;
    for (const resource of resources)
      this.revisions.set(keyOf(resource), this.revision);
    return this.event("change", resources);
  }
  publishEntitySequence(sequence: number) {
    const key = keyOf({ kind: "state" });
    if (sequence <= (this.revisions.get(key) ?? 0)) return null;
    this.revision++;
    this.revisions.set(key, sequence);
    const event = this.event("change", [{ kind: "state" }]);
    event.resourceVersions[0] = {
      resource: { kind: "state" },
      revision: sequence,
      entitySequences: [sequence],
    };
    return event;
  }
}

class Source {
  static instances: Source[] = [];
  static hub: Hub;
  listeners = new Map<string, Set<(event: MessageEvent<string>) => void>>();
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  closed = false;
  resources: Ref[];
  constructor(readonly url: string) {
    Source.instances.push(this);
    this.resources = JSON.parse(
      new URL(url, "http://studio.test").searchParams.get("resources") || "[]",
    );
    // The server builds the baseline when the request arrives.
    setTimeout(() => {
      if (!this.closed)
        this.emit("resources", Source.hub.event("initial", this.resources));
    }, 5);
  }
  addEventListener(
    name: string,
    listener: (event: MessageEvent<string>) => void,
  ) {
    if (name === "api-schema") {
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
    if (this.closed) return;
    const event = { data: JSON.stringify(value) } as MessageEvent<string>;
    for (const listener of this.listeners.get(name) || []) listener(event);
  }
  subscribed(ref: Ref) {
    return this.resources.some((item) => keyOf(item) === keyOf(ref));
  }
  close() {
    this.closed = true;
  }
}

const live = () => Source.instances.filter((source) => !source.closed);
// Deliver a hub event only to streams subscribed to one of its resources.
const deliver = (event: ReturnType<Hub["event"]> | null) => {
  if (!event) return 0;
  let delivered = 0;
  for (const source of live()) {
    const resources = event.resources.filter((ref) => source.subscribed(ref));
    if (!resources.length) continue;
    source.emit("resources", {
      ...event,
      resources,
      resourceVersions: event.resourceVersions.filter((entry, index) =>
        source.subscribed(entry.resource ?? event.resources[index]!),
      ),
    });
    delivered++;
  }
  return delivered;
};

type Deferred = {
  resolve: (response: Response) => void;
  reject: (e: unknown) => void;
};

async function setup(entitySequence = 0) {
  vi.useFakeTimers();
  vi.spyOn(Math, "random").mockReturnValue(0.5);
  syncDatabase.mockResolvedValue({ workspaceId });
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("document", {
    hidden: false,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("location", { origin: "http://studio.test" });
  vi.stubGlobal("crypto", { randomUUID: () => "tab-one" });
  vi.stubGlobal("EventSource", Source);
  const hub = new Hub(entitySequence);
  Source.hub = hub;
  const posts: Deferred[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(
      (_request: Request) =>
        new Promise<Response>((resolve, reject) =>
          posts.push({ resolve, reject }),
        ),
    ),
  );
  const api = await import("./api");
  api.setToken("test-token");
  const events = await import("./sync/resourceEvents");
  const acknowledge = events.acknowledgeEntitySequences;
  // sync/client.ts is mocked; this is its head listener for response envelopes.
  window.addEventListener("codex-sync-entities", (event: Event) => {
    const detail = (event as CustomEvent).detail;
    const sequences = (detail.documents as { seq: number }[]).map(
      (document) => document.seq,
    );
    acknowledge(sequences, detail.workspaceId);
  });
  api.setWorkspace(workspaceId);
  const post = () =>
    api
      .post("/api/stop" as never, { id: "x" } as never)
      .catch((error: unknown) => error);
  return { hub, posts, post, watch: events.watchResourceChanges };
}

const tick = (ms = 300) => vi.advanceTimersByTimeAsync(ms);
const state: Ref = { kind: "state" };
const accounts: Ref = { kind: "accounts" };
const costs: Ref = { kind: "costs" };
const envelope = (...sequences: number[]) =>
  Response.json({
    ok: true,
    _syncEntities: sequences.map((seq) => ({
      id: `entity:agent:a${seq}`,
      seq,
      payload: "{}",
      _deleted: false,
    })),
  });

describe("round two: freshness with the real hub revision rules", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    Source.instances = [];
    syncDatabase.mockClear();
    resumeListeners.clear();
  });

  it("F1 reconnect after missed events re-reads changed and unchanged resources", async () => {
    const { hub, watch } = await setup();
    let a = 0;
    let c = 0;
    watch(accounts as never, () => a++);
    watch(costs as never, () => c++);
    await tick();
    const base = [a, c];
    live()[0]!.onerror?.();
    hub.publishMany([accounts]); // nobody connected
    await tick(3000);
    console.log(
      "F1",
      JSON.stringify({ base, after: [a, c], streams: Source.instances.length }),
    );
    expect(a).toBeGreaterThan(base[0]!);
    expect(c).toBeGreaterThan(base[1]!);
  });

  it("F2 a server restart (new epoch) re-reads every resource", async () => {
    const { watch } = await setup();
    let a = 0;
    let c = 0;
    watch(accounts as never, () => a++);
    watch(costs as never, () => c++);
    await tick();
    const base = [a, c];
    live()[0]!.onerror?.();
    const restarted = new Hub();
    restarted.epoch = "epoch-2";
    Source.hub = restarted;
    await tick(3000);
    console.log("F2", JSON.stringify({ base, after: [a, c] }));
    expect(a).toBeGreaterThan(base[0]!);
    expect(c).toBeGreaterThan(base[1]!);
  });

  it("F3 a resource that left and re-entered after changing meanwhile is re-read", async () => {
    const { hub, watch } = await setup();
    let c = 0;
    watch(accounts as never, () => {});
    const stop = watch(costs as never, () => c++);
    await tick();
    stop();
    await tick();
    const base = c;
    deliver(hub.publishMany([costs])); // not subscribed: no frame
    watch(costs as never, () => c++);
    await tick();
    console.log("F3", JSON.stringify({ base, after: c }));
    expect(c).toBeGreaterThan(base);
  });

  it("F4 a change in the gap between closing the old stream and the new baseline is re-read", async () => {
    const { hub, watch } = await setup();
    let a = 0;
    watch(accounts as never, () => a++);
    await tick();
    const base = a;
    watch(costs as never, () => {}); // reconfigure: old stream closes at once
    await tick(0);
    hub.publishMany([accounts]); // committed before the new stream is registered
    deliver(null);
    await tick();
    console.log(
      "F4",
      JSON.stringify({ base, after: a, streams: Source.instances.length }),
    );
    expect(a).toBeGreaterThan(base);
  });

  it("F5 two changes to one resource between reconfigures cause one re-read", async () => {
    const { hub, watch } = await setup();
    let a = 0;
    watch(accounts as never, () => a++);
    await tick();
    const base = a;
    live()[0]!.close();
    hub.publishMany([accounts]);
    hub.publishMany([accounts]);
    watch(costs as never, () => {});
    await tick();
    console.log("F5", JSON.stringify({ base, after: a }));
    expect(a - base).toBe(1);
  });

  it("F6 idle-stop grace: a change while no watcher exists is re-read on return within one second", async () => {
    const { hub, watch } = await setup();
    let a = 0;
    const stop = watch(accounts as never, () => a++);
    await tick();
    stop();
    await tick(100);
    const openDuringGrace = live().length;
    hub.publishMany([accounts]);
    const base = a;
    watch(accounts as never, () => a++);
    await tick(600);
    console.log("F6", JSON.stringify({ openDuringGrace, base, after: a }));
    expect(a).toBeGreaterThan(base);
  });

  it("F7 an unchanged navigation does not re-read state (the claimed saving)", async () => {
    const { hub, watch } = await setup(40);
    let s = 0;
    watch(state as never, () => s++);
    await tick();
    const base = s;
    hub.publishMany([{ kind: "terminals" }]);
    watch(costs as never, () => {});
    await tick();
    console.log(
      "F7",
      JSON.stringify({ base, after: s, streams: Source.instances.length }),
    );
    expect(s).toBe(base);
  });
});

describe("round two: acting-tab suppression", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    Source.instances = [];
    syncDatabase.mockClear();
    resumeListeners.clear();
  });

  it("S1 own rows only: the matching event causes no pull", async () => {
    const { hub, posts, post, watch } = await setup(40);
    let s = 0;
    watch(state as never, () => s++);
    await tick();
    const base = s;
    const pending = post();
    await tick(1);
    posts[0]!.resolve(envelope(41));
    await pending;
    // The response envelope acknowledges this exact sequence batch before its
    // trailing StateResource event reaches the tab.
    deliver(hub.publishEntitySequence(41));
    await tick(50);
    console.log("S1", JSON.stringify({ pulls: s - base }));
    expect(s - base).toBe(0);
  });

  it("S2 the POST fails after the server committed: the held event is released", async () => {
    const { hub, posts, post, watch } = await setup(40);
    let s = 0;
    watch(state as never, () => s++);
    await tick();
    const base = s;
    const pending = post();
    await tick(1);
    deliver(hub.publishEntitySequence(41));
    await tick(50);
    posts[0]!.reject(new TypeError("network lost"));
    await pending;
    await tick();
    console.log("S2", JSON.stringify({ pulls: s - base }));
    expect(s - base).toBe(1);
  });

  it("S3 another writer's commit just before this tab's POST is not lost", async () => {
    // Order on the server: other writer commits 41; this tab's POST samples
    // after=41 and commits 42; the envelope carries only 42 (seq > 41).
    const { hub, posts, post, watch } = await setup(40);
    let s = 0;
    watch(state as never, () => s++);
    await tick();
    const base = s;
    const pending = post(); // request leaves the tab
    await tick(1);
    deliver(hub.publishEntitySequence(41)); // other writer's event arrives, is held
    deliver(hub.publishEntitySequence(42)); // own commit
    await tick(50);
    posts[0]!.resolve(envelope(42));
    await pending;
    await tick(5000);
    console.log(
      "S3",
      JSON.stringify({
        pulls: s - base,
        note: "row 41 is in neither the envelope nor a pull",
      }),
    );
    expect(s - base).toBeGreaterThanOrEqual(1);
  });

  it("S4 another writer's event within the 20 ms flush window before the response is not lost", async () => {
    const { hub, posts, post, watch } = await setup(40);
    let s = 0;
    watch(state as never, () => s++);
    await tick();
    const base = s;
    const pending = post();
    await tick(200);
    deliver(hub.publishEntitySequence(41));
    await tick(5);
    deliver(hub.publishEntitySequence(42));
    posts[0]!.resolve(envelope(42));
    await pending;
    await tick(5000);
    console.log("S4", JSON.stringify({ pulls: s - base }));
    expect(s - base).toBeGreaterThanOrEqual(1);
  });

  it("S5 a POST that never settles does not block other writers' state events", async () => {
    const { hub, post, watch } = await setup(40);
    let s = 0;
    watch(state as never, () => s++);
    await tick();
    const base = s;
    void post(); // no timeoutMs: post() has no default deadline
    await tick(1);
    deliver(hub.publishEntitySequence(41)); // another tab's change
    await tick(60_000);
    deliver(hub.publishEntitySequence(42));
    await tick(600_000);
    console.log("S5", JSON.stringify({ pullsAfter11Minutes: s - base }));
    expect(s - base).toBeGreaterThanOrEqual(1);
  });
});
