import { afterEach, expect, it, vi } from "vitest";
import { nativeEventResponse, NATIVE_EVENT_MAX_BYTES } from "./stream";
import { SignedEventSource } from "../servers/eventSource";
import { SYNC_STREAM_EVENT_MAX_BYTES } from "../generated/sharedValues";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";

afterEach(() => vi.unstubAllGlobals());
function setup(options: { rejectGrant?: boolean } = {}) {
  let emit: (event: { payload: unknown }) => void;
  let subscription: string;
  let acknowledge: () => void;
  let started: () => void;
  let closed: () => void;
  const startedPromise = new Promise<void>((resolve) => {
    started = resolve;
  });
  const closedPromise = new Promise<void>((resolve) => {
    closed = resolve;
  });
  const unlisten = vi.fn();
  const requested = Promise.withResolvers<void>();
  let credit = "credit-0";
  let granted: string | undefined;
  let sequence = 0;
  const queued: unknown[] = [];
  const flush = () => {
    if (!granted || !queued.length) return;
    const consumedCredit = granted;
    granted = undefined;
    credit = `credit-${++sequence}`;
    emit({
      payload: {
        event: subscription,
        consumedCredit,
        credit,
        value: queued.shift(),
      },
    });
  };
  const invoke = vi.fn(async (_command, { request }) => {
    if (request.method === "view.subscribe") {
      subscription = request.params.subscription;
      started();
      return new Promise((resolve) => {
        acknowledge = () =>
          resolve({ id: request.id, result: { subscribed: true } });
      });
    }
    if (request.method === "view.next") {
      requested.resolve();
      if (options.rejectGrant) throw new Error("The grant response was lost.");
      if (request.params.credit !== credit || granted)
        throw new Error("Invalid credit");
      granted = credit;
      flush();
      return { id: request.id, result: { granted: true } };
    }
    closed();
    return { id: request.id, result: { closed: true } };
  });
  vi.stubGlobal("__TAURI__", {
    core: { invoke },
    event: {
      listen: async (_name: string, callback: typeof emit) => {
        emit = callback;
        return unlisten;
      },
    },
  });
  return {
    started: startedPromise,
    closed: closedPromise,
    invoke,
    unlisten,
    requested: requested.promise,
    event: (value: { type: string; data?: string; [key: string]: unknown }) => {
      if (value.data !== undefined) {
        queued.push(value);
        flush();
      } else emit({ payload: { event: subscription, credit, value } });
    },
    raw: (value: unknown, consumedCredit?: string, nextCredit = "foreign") =>
      emit({
        payload: {
          event: subscription,
          value,
          consumedCredit,
          credit: nextCredit,
        },
      }),
    acknowledge: () => acknowledge(),
  };
}

it("keeps a native initial transfer larger than the queue bound while its consumer holds", async () => {
  vi.stubGlobal("location", { origin: "http://localhost", search: "" });
  const host = setup();
  const held = Promise.withResolvers<void>();
  const first = Promise.withResolvers<void>();
  const settled = Promise.withResolvers<void>();
  const readFailures: unknown[] = [];
  const frames = Array.from({ length: 20 }, (_, index) =>
    JSON.stringify({
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: "a".repeat(32),
      epoch: "initial",
      revision: 1,
      reason: "initial",
      resource: { kind: "state" },
      cursor: { workspaceId: "a".repeat(32), seq: 20 },
      replyKind: "snapshot",
      transferId: "native-initial-transfer",
      pageIndex: index,
      partIndex: 0,
      partCount: 1,
      complete: index === 19,
      data: JSON.stringify({
        after: index,
        through: index + 1,
        documents: [
          {
            collection: "workspace",
            id: `workspace-${index}`,
            seq: index + 1,
            op: "upsert",
            value: { stateDir: "x".repeat(240 * 1024) },
          },
        ],
      }),
    }),
  );
  expect(
    frames.every(
      (frame) =>
        new TextEncoder().encode(frame).byteLength <
        SYNC_STREAM_EVENT_MAX_BYTES,
    ),
  ).toBe(true);
  expect(
    frames.reduce(
      (size, frame) => size + new TextEncoder().encode(frame).byteLength,
      0,
    ),
  ).toBeGreaterThan(NATIVE_EVENT_MAX_BYTES);
  const source = new SignedEventSource("/api/sync/stream", async (request) => {
    const response = await nativeEventResponse(request, "/api/sync/stream");
    const getReader = response.body!.getReader.bind(
      response.body,
    ) as () => ReadableStreamDefaultReader<Uint8Array<ArrayBuffer>>;
    vi.spyOn(response.body!, "getReader").mockImplementation(() => {
      const reader = getReader();
      const read = reader.read.bind(reader);
      reader.read = async () => {
        try {
          return await read();
        } catch (error) {
          readFailures.push(error);
          throw error;
        }
      };
      return reader;
    });
    return response;
  });
  const delivered: string[] = [];
  source.addEventListener("entity-transfer", (event) => {
    delivered.push((event as MessageEvent<string>).data);
    if (delivered.length === 1) {
      source.holdUntil(held.promise);
      first.resolve();
    }
    if (delivered.length === frames.length) settled.resolve();
  });
  const failed = vi.fn(() => settled.resolve());
  source.onerror = failed;
  try {
    await host.started;
    host.acknowledge();
    host.event({ type: "open" });
    host.event({ type: "entity-transfer", data: frames[0] });
    await first.promise;
    for (const data of frames.slice(1))
      host.event({ type: "entity-transfer", data });
    expect(
      host.invoke.mock.calls.filter(
        (call) => call[1].request.method === "view.next",
      ),
    ).toHaveLength(1);
    held.resolve();
    await settled.promise;
    expect(readFailures.map(String)).toEqual([]);
    expect(failed).not.toHaveBeenCalled();
    expect(delivered).toEqual(frames);
  } finally {
    held.resolve();
    source.close();
    await host.closed;
  }
});
it("rejects an oversized individual event without increasing the stream buffer", async () => {
  const host = setup();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "open" });
  const reader = (await result).body!.getReader();
  const read = reader.read();
  host.event({ type: "message", data: "x".repeat(NATIVE_EVENT_MAX_BYTES) });
  await expect(read).rejects.toThrow(
    "The native event exceeds its size limit.",
  );
  await host.closed;
});
it("rejects a stale data credit during a later read", async () => {
  const host = setup();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "open" });
  const reader = (await result).body!.getReader();
  host.event({ type: "message", data: "first" });
  await reader.read();
  const second = reader.read();
  host.raw({ type: "message", data: "old" }, "credit-0", "credit-old");
  await expect(second).rejects.toThrow("The native event credit is invalid.");
  await host.closed;
});
it("rejects data without consumer demand", async () => {
  const host = setup();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "open" });
  const response = await result;
  host.raw({ type: "message", data: "unsolicited" }, "credit-0", "credit-1");
  await expect(response.body!.getReader().read()).rejects.toThrow(
    "The native event credit is invalid.",
  );
  await host.closed;
  expect(
    host.invoke.mock.calls.filter(
      (call) => call[1].request.method === "view.next",
    ),
  ).toHaveLength(0);
});
it("closes after a lost grant response without retrying it", async () => {
  const host = setup({ rejectGrant: true });
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "open" });
  const reader = (await result).body!.getReader();
  await expect(reader.read()).rejects.toThrow("The grant response was lost.");
  await host.closed;
  expect(
    host.invoke.mock.calls.filter(
      (call) => call[1].request.method === "view.next",
    ),
  ).toHaveLength(1);
});
it("cancels a pending credit wait and discards its late event", async () => {
  const host = setup();
  const controller = new AbortController();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events", { signal: controller.signal }),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "open" });
  const reader = (await result).body!.getReader();
  const read = reader.read();
  await host.requested;
  controller.abort();
  await expect(read).rejects.toMatchObject({ name: "AbortError" });
  host.event({ type: "message", data: "late" });
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledTimes(1);
});
it("carries multiline Unicode events and releases the stream on cancellation", async () => {
  const host = setup();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "open" });
  const response = await result;
  const reader = response.body!.getReader();
  host.event({ type: "updated", data: "Привет 🦀\nsecond", id: "rev-1" });
  expect(new TextDecoder().decode((await reader.read()).value)).toBe(
    "id: rev-1\nevent: updated\ndata: Привет 🦀\ndata: second\n\n",
  );
  host.event({ type: "error", data: "application event", id: "rev-2" });
  expect(new TextDecoder().decode((await reader.read()).value)).toBe(
    "id: rev-2\nevent: error\ndata: application event\n\n",
  );
  await reader.cancel();
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledTimes(1);
  expect(
    host.invoke.mock.calls.filter(
      (call) => call[1].request.method === "view.unsubscribe",
    ),
  ).toHaveLength(1);
});
it("closes a subscription acknowledged after the caller already aborted", async () => {
  const host = setup();
  const controller = new AbortController();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events", { signal: controller.signal }),
    "/api/events",
  );
  await host.started;
  controller.abort();
  await expect(result).rejects.toMatchObject({ name: "AbortError" });
  host.acknowledge();
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledTimes(1);
  expect(
    host.invoke.mock.calls.filter(
      (call) => call[1].request.method === "view.unsubscribe",
    ),
  ).toHaveLength(1);
});
it("rejects a failed stream handshake without reporting an HTTP success", async () => {
  const host = setup();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "error", message: "Backend denied access" });
  await expect(result).rejects.toThrow("Backend denied access");
  expect(host.unlisten).toHaveBeenCalledTimes(1);
});
