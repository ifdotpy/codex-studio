import { afterEach, expect, it, vi } from "vitest";
import type { NativeServerContext, NativeSubscriptionEvent } from "./client";
import { createNativeOutbox } from "./outbox";

const hooks = vi.hoisted(() => ({
  snapshots: [] as unknown[],
  stops: [] as (() => void)[],
}));
vi.mock("react", () => ({
  useSyncExternalStore: (
    subscribe: (changed: () => void) => () => void,
    snapshot: () => unknown,
  ) => {
    hooks.stops.push(subscribe(() => hooks.snapshots.push(snapshot())));
    return snapshot();
  },
}));

afterEach(() => {
  for (const stop of hooks.stops.splice(0)) stop();
  hooks.snapshots.length = 0;
  vi.unstubAllGlobals();
});

const workspace = "a".repeat(32);
const body = { id: "original-message", room: "room", text: "original input" };
function entry(overrides: Record<string, unknown> = {}) {
  return {
    id: body.id,
    workspace,
    body: { ...body },
    status: "queued",
    attempted: false,
    created: 1,
    displayPending: true,
    attachments: [],
    ...overrides,
  };
}
type Call = { method: string; params: Record<string, unknown>; id?: string };
type Reply = (call: Call) => unknown | Promise<unknown>;

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, item]) => [key, canonical(item)]),
  );
}

/** IPC fixtures only. Rust owns the actual queue transitions and HTTP claim. */
function context(reply: Reply, serverId = "remote") {
  const calls: Call[] = [];
  const stable = new Map<string, string>();
  const writes = new Map<string, { serialized: string; id: string }>();
  const saved = new Map<
    string,
    { signature: string; result: Promise<unknown> }
  >();
  const lostResponses = new Set<string>();
  const durable = new Set([
    "outbox.enqueue",
    "outbox.change",
    "outbox.display",
    "outbox.acknowledge",
    "outbox.acknowledge_many",
    "outbox.stop_removed",
  ]);
  let identity = 0;
  let receive: ((event: NativeSubscriptionEvent<unknown>) => void) | undefined;
  let opened!: () => void;
  const subscribed = new Promise<void>((resolve) => (opened = resolve));
  const close = vi.fn(async () => {});
  const native: Pick<
    NativeServerContext,
    "serverId" | "workspaceId" | "request" | "subscribe"
  > = Object.freeze({
    serverId,
    workspaceId: workspace,
    async request<T>(method: string, input: unknown, id?: string): Promise<T> {
      if (!input || typeof input !== "object" || Array.isArray(input))
        throw new Error("A native parameter object is required.");
      const params = structuredClone(input) as Record<string, unknown>;
      calls.push({ method, params, id });
      if (method === "system.uuid") return `native:${++identity}` as T;
      if (method === "mutation.stable_id") {
        const key = params.key;
        if (typeof key !== "string" || !key)
          throw new Error("An exact stable key is required.");
        if (!stable.has(key)) stable.set(key, `saved:${++identity}`);
        return stable.get(key) as T;
      }
      if (method === "mutation.clear_stable_id") {
        const key = String(params.key);
        const matches = stable.get(key) === params.requestId;
        if (matches) stable.delete(key);
        return matches as T;
      }
      if (
        method === "mutation.write_id" ||
        method === "mutation.clear_write_id"
      ) {
        const key = params.key;
        if (
          typeof key !== "string" ||
          !key ||
          new TextEncoder().encode(key).length > 4096
        )
          throw new Error("A bounded write key is required.");
        if (method === "mutation.clear_write_id") {
          const matches = writes.get(key)?.id === params.requestId;
          if (matches) writes.delete(key);
          return matches as T;
        }
        const serialized = params.serializedPayload;
        if (typeof serialized !== "string")
          throw new Error("An exact serialized write is required.");
        const envelope = JSON.parse(serialized);
        if (
          !durable.has(envelope.method) ||
          !envelope.params ||
          typeof envelope.params !== "object"
        )
          throw new Error("A durable outbox payload is required.");
        let current = writes.get(key);
        if (current?.serialized !== serialized) {
          current = { serialized, id: `saved:${++identity}` };
          writes.set(key, current);
        }
        return current!.id as T;
      }
      if (!method.startsWith("outbox."))
        throw new Error(`Unexpected non-outbox command ${method}`);
      if (method !== "outbox.activate" && params.workspace !== workspace)
        throw new Error("The queue command crossed its workspace.");
      let result: unknown;
      if (durable.has(method)) {
        if (!id) throw new Error("An exact native request ID is required.");
        const signature = JSON.stringify(canonical({ method, params }));
        let previous = saved.get(id);
        if (previous && previous.signature !== signature)
          throw new Error("identity_conflict");
        if (!previous) {
          previous = {
            signature,
            result: Promise.resolve().then(() => reply({ method, params, id })),
          };
          saved.set(id, previous);
        }
        result = await previous.result;
      } else result = await reply({ method, params, id });
      if (lostResponses.delete(method)) throw new Error("lost native response");
      return result as T;
    },
    async subscribe<T>(
      topic: string,
      params: Record<string, unknown>,
      callback: (event: NativeSubscriptionEvent<T>) => void,
    ) {
      expect(topic).toBe("outbox");
      expect(params).toEqual({});
      receive = (event) => callback(event as NativeSubscriptionEvent<T>);
      opened();
      return close;
    },
  });
  return {
    native,
    calls,
    subscribed,
    close,
    loseNextResponse(method: string) {
      lostResponses.add(method);
    },
    emit(value: unknown) {
      if (!receive) throw new Error("The native view is not open.");
      receive({ value });
    },
    fail(message: string) {
      if (!receive) throw new Error("The native view is not open.");
      receive({ error: { code: "fixture", message } });
    },
  };
}

it("commits the original message before onPersist and delegates delivery to Rust", async () => {
  const order: string[] = [];
  const saved = entry();
  const receipt = { id: body.id, status: "delivered", deliveries: {} };
  const accepted = entry({
    status: "accepted",
    attempted: true,
    receipt,
  });
  let current = saved;
  const f = context(({ method, params }) => {
    if (method === "outbox.enqueue") {
      expect(params).toEqual({ workspace, body, attachments: [] });
      order.push("committed");
      return saved;
    }
    if (method === "outbox.deliver") {
      expect(params).toEqual({ workspace, id: body.id });
      order.push("native delivery");
      current = accepted;
      return receipt;
    }
    if (method === "outbox.list") return [current];
    throw new Error(`Unexpected command ${method}`);
  });
  const result = await createNativeOutbox(f.native).durableSend(
    body,
    [],
    () => {
      order.push("onPersist");
    },
  );
  expect(order).toEqual(["committed", "onPersist", "native delivery"]);
  expect(result).toMatchObject({
    id: body.id,
    kind: "server",
    status: "delivered",
  });
  const enqueue = f.calls.find((call) => call.method === "outbox.enqueue");
  expect(enqueue?.id).toBeTruthy();
  expect(
    f.calls.filter((call) => call.method === "outbox.deliver"),
  ).toHaveLength(1);
});

it("reads an uncertain delivery once without a new message or renderer retry", async () => {
  const saved = entry({ attempted: true, error: "native response unknown" });
  const f = context(({ method, params }) => {
    if (method === "outbox.enqueue") return saved;
    if (method === "outbox.deliver") {
      expect(params.id).toBe(body.id);
      throw new Error("lost delivery reply");
    }
    if (method === "outbox.list") return [saved];
    throw new Error(`Unexpected command ${method}`);
  });
  const persist = vi.fn();
  const result = await createNativeOutbox(f.native).durableSend(
    body,
    [],
    persist,
  );
  expect(persist).toHaveBeenCalledTimes(1);
  expect(result).toMatchObject({
    kind: "local",
    id: body.id,
    status: "queued",
  });
  expect(
    f.calls.filter((call) => call.method === "outbox.enqueue"),
  ).toHaveLength(1);
  expect(
    f.calls.filter((call) => call.method === "outbox.deliver"),
  ).toHaveLength(1);
  expect(f.calls.filter((call) => call.method === "outbox.list")).toHaveLength(
    1,
  );
});

it("keeps the original input while Rust returns reordered JSON object keys", async () => {
  const input = { ...body };
  const receipt = { id: body.id, status: "delivered" };
  const saved = entry({
    body: { text: body.text, room: body.room, id: body.id },
    status: "accepted",
    receipt,
  });
  const f = context(({ method, params }) => {
    if (method === "outbox.enqueue") {
      expect(params.body).toEqual(body);
      return saved;
    }
    throw new Error(`Unexpected command ${method}`);
  });
  const sent = createNativeOutbox(f.native).durableSend(input);
  input.text = "later edit";
  expect(await sent).toMatchObject({
    id: body.id,
    status: "delivered",
    kind: "server",
  });
  expect(f.calls.some((call) => call.method === "outbox.deliver")).toBe(false);
});

it("routes display, acknowledgement and queue actions through the exact owner", async () => {
  const f = context(({ method, params }) => {
    if (method === "outbox.acknowledge_many") return { acknowledged: true };
    if (method === "outbox.display" || method === "outbox.change")
      return entry({ displayText: params.text, status: "paused" });
    throw new Error(`Unexpected command ${method}`);
  });
  const queue = createNativeOutbox(f.native);
  await queue.acknowledgeOutbox([body.id]);
  await queue.editOutboxDisplay(body.id, "display only");
  await queue.changeOutbox(body.id, "pause");
  expect(f.calls.filter((call) => call.method.startsWith("outbox."))).toEqual([
    expect.objectContaining({
      method: "outbox.acknowledge_many",
      params: { workspace, ids: [body.id] },
    }),
    expect.objectContaining({
      method: "outbox.display",
      params: { workspace, id: body.id, text: "display only" },
    }),
    expect.objectContaining({
      method: "outbox.change",
      params: { workspace, id: body.id, action: "pause" },
    }),
  ]);
});

it("uses a different exact identity for resume after a committed pause loses its response", async () => {
  const f = context(({ method, params }) => {
    if (method !== "outbox.change")
      throw new Error(`Unexpected command ${method}`);
    return entry({
      status: params.action === "pause" ? "paused" : "queued",
      attempted: true,
    });
  });
  const queue = createNativeOutbox(f.native);
  f.loseNextResponse("outbox.change");
  await expect(queue.changeOutbox(body.id, "pause")).rejects.toThrow(
    "lost native response",
  );
  await queue.changeOutbox(body.id, "resume");
  await queue.changeOutbox(body.id, "pause");
  const changes = f.calls.filter((call) => call.method === "outbox.change");
  expect(changes.map((call) => call.params.action)).toEqual([
    "pause",
    "resume",
    "pause",
  ]);
  expect(changes[1]?.id).not.toBe(changes[0]?.id);
  expect(changes[2]?.id).not.toBe(changes[0]?.id);
  expect(changes[2]?.id).not.toBe(changes[1]?.id);
});

it("keeps large display payloads outside the bounded native identity key", async () => {
  const text = "x".repeat(8192);
  const f = context(({ method, params }) => {
    if (method === "outbox.display") return entry({ displayText: params.text });
    throw new Error(`Unexpected command ${method}`);
  });
  await createNativeOutbox(f.native).editOutboxDisplay(body.id, text);
  const preparation = f.calls.find(
    (call) => call.method === "mutation.write_id",
  );
  expect(preparation).toBeDefined();
  expect(JSON.parse(String(preparation?.params.serializedPayload))).toEqual({
    method: "outbox.display",
    params: { workspace, id: body.id, text },
  });
  expect(String(preparation?.params.key)).not.toContain(text);
});

it("does not clear the current write identity with an old confirmation", async () => {
  const f = context(({ method, params }) => {
    if (method === "outbox.change")
      return entry({ status: params.action === "pause" ? "paused" : "queued" });
    throw new Error(`Unexpected command ${method}`);
  });
  const queue = createNativeOutbox(f.native);
  f.loseNextResponse("outbox.change");
  await expect(queue.changeOutbox(body.id, "pause")).rejects.toThrow(
    "lost native response",
  );
  f.loseNextResponse("outbox.change");
  await expect(queue.changeOutbox(body.id, "resume")).rejects.toThrow(
    "lost native response",
  );
  const changes = f.calls.filter((call) => call.method === "outbox.change");
  const keys = f.calls.filter((call) => call.method === "mutation.write_id");
  expect(keys[0]?.params.key).toBe(keys[1]?.params.key);
  expect(
    await f.native.request("mutation.clear_write_id", {
      key: keys[0]?.params.key,
      requestId: changes[0]?.id,
    }),
  ).toBe(false);
  await queue.changeOutbox(body.id, "resume");
  const retried = f.calls.filter((call) => call.method === "outbox.change");
  expect(retried[2]?.id).toBe(changes[1]?.id);
});

it("keeps native preparation, local write and confirmation in explicit action order", async () => {
  const f = context(({ method, params }) => {
    if (method === "outbox.change")
      return entry({ status: params.action === "pause" ? "paused" : "queued" });
    throw new Error(`Unexpected command ${method}`);
  });
  let release!: () => void;
  let prepared!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  const firstPrepared = new Promise<void>((resolve) => (prepared = resolve));
  let first = true;
  const queue = createNativeOutbox({
    ...f.native,
    async request<T>(method: string, params: unknown, id?: string) {
      const result = await f.native.request<T>(method, params, id);
      if (method === "mutation.write_id" && first) {
        first = false;
        prepared();
        await held;
      }
      return result;
    },
  });
  const pause = queue.changeOutbox(body.id, "pause");
  const resume = queue.changeOutbox(body.id, "resume");
  await firstPrepared;
  release();
  await Promise.all([pause, resume]);
  expect(f.calls.map((call) => call.method)).toEqual([
    "mutation.write_id",
    "outbox.change",
    "mutation.clear_write_id",
    "mutation.write_id",
    "outbox.change",
    "mutation.clear_write_id",
  ]);
});

it("uses separate exact identities for concurrent acknowledgement payloads", async () => {
  const f = context(({ method }) => {
    if (method === "outbox.acknowledge_many") return { acknowledged: true };
    throw new Error(`Unexpected command ${method}`);
  });
  const queue = createNativeOutbox(f.native);
  await Promise.all([
    queue.acknowledgeOutbox(["first-message"]),
    queue.acknowledgeOutbox(["second-message"]),
  ]);
  const acknowledgements = f.calls.filter(
    (call) => call.method === "outbox.acknowledge_many",
  );
  expect(acknowledgements.map((call) => call.params.ids)).toEqual([
    ["first-message"],
    ["second-message"],
  ]);
  expect(acknowledgements[1]?.id).not.toBe(acknowledgements[0]?.id);
});

it("rejects another workspace's receipt reconciliation before any native mutation", async () => {
  const f = context(() => {
    throw new Error("No native request is allowed.");
  });
  await createNativeOutbox(f.native).reconcileOutboxReceipts(
    body.room,
    [
      {
        id: body.id,
        agent: body.room,
        status: "delivered",
        materialized: true,
        deliveries: {},
      },
    ],
    "b".repeat(32),
  );
  expect(f.calls).toEqual([]);
});

it("reconciles only matching acknowledged rows and retires materialized delivery", async () => {
  const receipt = {
    id: body.id,
    agent: body.room,
    status: "delivered",
    materialized: true,
    deliveries: {},
  } as const;
  const f = context(({ method, params }) => {
    if (method === "outbox.list")
      return [entry({ status: "uncertain", attempted: true })];
    if (method === "outbox.receipt") {
      expect(params).toEqual({ workspace, id: body.id, receipt });
      return entry({ status: "accepted", attempted: true, receipt });
    }
    if (method === "outbox.acknowledge_many") return { acknowledged: true };
    throw new Error(`Unexpected command ${method}`);
  });
  await createNativeOutbox(f.native).reconcileOutboxReceipts(
    body.room,
    [{ ...receipt, agent: "another-room" }, receipt],
    workspace,
  );
  expect(
    f.calls
      .filter((call) => call.method.startsWith("outbox."))
      .map((call) => call.method),
  ).toEqual(["outbox.list", "outbox.receipt", "outbox.acknowledge_many"]);
});

it.each(["queued", "paused"])(
  "settles an attempted %s native row from its exact server receipt",
  async (status) => {
    const receipt = {
      id: body.id,
      agent: body.room,
      status: "delivered",
      materialized: false,
      deliveries: {},
    } as const;
    const f = context(({ method, params }) => {
      // Native delivery keeps attempted=true after a lost server response.
      if (method === "outbox.list")
        return [entry({ status, attempted: true, error: "response unknown" })];
      if (method === "outbox.receipt") {
        expect(params).toEqual({ workspace, id: body.id, receipt });
        return entry({ status: "accepted", attempted: true, receipt });
      }
      throw new Error(`Unexpected command ${method}`);
    });
    await createNativeOutbox(f.native).reconcileOutboxReceipts(
      body.room,
      [receipt],
      workspace,
    );
    expect(f.calls.map((call) => call.method)).toEqual([
      "outbox.list",
      "outbox.receipt",
    ]);
  },
);

it("retains the exact native removal identity after an uncertain fence response", async () => {
  const f = context(({ method, params, id }) => {
    if (method !== "outbox.stop_removed")
      throw new Error(`Unexpected command ${method}`);
    expect(params).toEqual({ workspace, id: body.id });
    expect(id).toBeTruthy();
    return false;
  });
  const queue = createNativeOutbox(f.native);
  f.loseNextResponse("outbox.stop_removed");
  await expect(queue.stopRemovedMessage(body.id)).rejects.toThrow(
    "lost native response",
  );
  expect(
    f.calls.filter((call) => call.method === "mutation.clear_write_id"),
  ).toHaveLength(0);
  expect(await queue.stopRemovedMessage(body.id)).toBe(false);
  const stops = f.calls.filter((call) => call.method === "outbox.stop_removed");
  expect(stops).toHaveLength(2);
  expect(stops[0]).toEqual(stops[1]);
  expect(await queue.stopRemovedMessage(body.id)).toBe(false);
  expect(
    f.calls.filter((call) => call.method === "outbox.stop_removed")[2]?.id,
  ).not.toBe(stops[0]?.id);
});

it("waits for both pending native subscriptions before disposal returns", async () => {
  const f = context(({ method }) => {
    if (method === "outbox.activate") return { active: true };
    throw new Error(`Unexpected command ${method}`);
  });
  type Close = () => Promise<void>;
  const pending: ((close: Close) => void)[] = [];
  const queue = createNativeOutbox({
    ...f.native,
    subscribe: async () =>
      new Promise<Close>((resolve) => pending.push(resolve)),
  });
  const firstClose = vi.fn(async () => {});
  const secondClose = vi.fn(async () => {});
  queue.useOutbox();
  hooks.stops.splice(0).forEach((stop) => stop());
  queue.useOutbox();
  expect(pending).toHaveLength(2);
  let returned = false;
  const disposal = queue.dispose().then(() => {
    returned = true;
  });
  pending[1]!(secondClose);
  // Complete the second open and its close without a timer or external event.
  for (let step = 0; step < 12; step++) await Promise.resolve();
  const returnedBeforeFirstOpen = returned;
  pending[0]!(firstClose);
  await disposal;
  for (let step = 0; step < 4; step++) await Promise.resolve();
  expect(returnedBeforeFirstOpen).toBe(false);
  expect(firstClose).toHaveBeenCalledTimes(1);
  expect(secondClose).toHaveBeenCalledTimes(1);
});

it("uses native outbox events without browser state or a renderer timer", async () => {
  for (const name of ["localStorage", "indexedDB", "fetch", "crypto"])
    vi.stubGlobal(name, undefined);
  const f = context(({ method }) => {
    if (method === "outbox.activate") return { active: true };
    throw new Error(`Unexpected command ${method}`);
  });
  const queue = createNativeOutbox(f.native);
  expect(queue.useOutbox()).toEqual({ entries: [], error: "" });
  await f.subscribed;
  f.emit({ workspaceId: workspace, entries: [entry()] });
  expect(hooks.snapshots.at(-1)).toEqual({ entries: [entry()], error: "" });
  f.emit({ workspaceId: "b".repeat(32), entries: [entry({ id: "foreign" })] });
  expect(hooks.snapshots.at(-1)).toMatchObject({ entries: [entry()] });
  f.fail("native view unavailable");
  expect(hooks.snapshots.at(-1)).toEqual({
    entries: [entry()],
    error: "native view unavailable",
  });
  for (const stop of hooks.stops.splice(0)) stop();
  await Promise.resolve();
  expect(f.close).toHaveBeenCalledTimes(1);
  expect(f.calls.map((call) => call.method)).toEqual(["outbox.activate"]);
});
