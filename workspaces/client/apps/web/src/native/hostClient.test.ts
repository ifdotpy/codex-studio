import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { fileURLToPath } from "node:url";
import { readFile } from "node:fs/promises";
import { checkNativeImportGraph } from "../../scripts/nativeImportGraph.mjs";

beforeEach(() => {
  vi.resetModules();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

function transport(
  reply?: (request: { id: string; method: string; params: any }) => unknown,
) {
  let identity = 0;
  const channels: TestChannel[] = [];
  class TestChannel {
    id: number;
    onmessage: (value: unknown) => void;
    constructor(receive: (value: unknown) => void) {
      this.id = channels.length + 1;
      this.onmessage = receive;
      channels.push(this);
    }
    toJSON() {
      return `__CHANNEL__:${this.id}`;
    }
  }
  const invoke = vi.fn(async (command: string, args: any) => {
    expect(command).toBe("client_request");
    const request = args.request;
    if (request.method === "system.uuid")
      return {
        id: request.id,
        result: `00000000-0000-4000-8000-${String(++identity).padStart(12, "0")}`,
      };
    if (reply) return reply(request);
    return {
      id: request.id,
      result:
        request.method === "ui.events.connect"
          ? { leaseId: "exact-lease" }
          : request.method === "ui.events.disconnect"
            ? { disconnected: true }
            : "saved",
    };
  });
  const eventListen = vi.fn(() => {
    throw new Error("event.listen is forbidden");
  });
  const current = { location: { protocol: "tauri:" } } as any;
  current.top = current;
  current.parent = current;
  vi.stubGlobal("window", current);
  vi.stubGlobal("__TAURI__", {
    core: { invoke, Channel: TestChannel },
    event: { listen: eventListen },
  });
  return {
    invoke,
    channels,
    eventListen,
    current,
    requests: () => invoke.mock.calls.map(([, args]) => args.request),
  };
}

it("resolves a host-only client graph without the frame transport", async () => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const report = await checkNativeImportGraph({
    root,
    entry: fileURLToPath(new URL("./hostClient.ts", import.meta.url)),
  });
  expect(report.findings).toEqual([]);
  expect(report.modules.some((name) => name.endsWith("native/frame.ts"))).toBe(
    false,
  );
});

it("uses native request identities and preserves exact uncertain writes without retry", async () => {
  let lost = true;
  const mock = transport(({ id }) => {
    if (lost) {
      lost = false;
      throw new Error("response lost");
    }
    return { id, result: null };
  });
  const client = await import("./hostClient");
  await expect(
    client.nativeRequest("outbox.enqueue", { body: "original" }, "exact-write"),
  ).rejects.toThrow("response lost");
  expect(mock.requests()).toHaveLength(1);
  await expect(
    client.nativeRequest("outbox.enqueue", { body: "original" }, "exact-write"),
  ).resolves.toBeNull();
  expect(mock.requests()[0]).toEqual(mock.requests()[1]);
  await client.nativeRequest("read", {});
  expect(mock.requests().at(-1).id).toMatch(/^[a-f0-9-]{36}$/);
});
it("keeps response identity and native command errors in the shared core", async () => {
  transport(({ id, method }) =>
    method === "wrong"
      ? { id: "foreign", result: true }
      : method === "ambiguous"
        ? { id, result: true, error: { message: "no" } }
        : {
            id,
            error: {
              message: "Permission denied",
              code: "authority",
              details: { outcome: "unknown" },
            },
          },
  );
  const client = await import("./hostClient");
  await expect(client.nativeRequest("wrong", {}, "write")).rejects.toThrow(
    "mismatched",
  );
  await expect(client.nativeRequest("ambiguous", {}, "write")).rejects.toThrow(
    "invalid response",
  );
  await expect(
    client.nativeRequest("denied", {}, "write"),
  ).rejects.toMatchObject({
    name: "NativeCommandError",
    code: "authority",
    details: { outcome: "unknown" },
  });
});
it("opens one document Channel and fans out all fixed topics", async () => {
  const mock = transport();
  const host = await import("./hostClient");
  const callbacks = host.HOST_EVENT_TOPICS.map(() => vi.fn());
  await Promise.all(
    host.HOST_EVENT_TOPICS.map((topic, index) =>
      host.listenHostEvent(topic, callbacks[index]),
    ),
  );
  expect(mock.channels).toHaveLength(1);
  expect(
    mock.requests().filter((request) => request.method === "ui.events.connect"),
  ).toMatchObject([{ params: { callback: "__CHANNEL__:1" } }]);
  host.HOST_EVENT_TOPICS.forEach((topic, index) => {
    mock.channels[0].onmessage({ event: topic, payload: { index } });
    expect(callbacks[index]).toHaveBeenCalledWith({ index });
  });
  expect(mock.eventListen).not.toHaveBeenCalled();
});
it("preserves the existing client-event payload for server subscriptions", async () => {
  const mock = transport(({ id, method, params }) => ({
    id,
    result:
      method === "ui.events.connect"
        ? { leaseId: "subscription-lease" }
        : method === "service.open"
          ? {
              handle: "owned",
              serverId: params.serverId,
              workspaceId: "a".repeat(32),
              origin: "https://owned.invalid",
              credentialId: null,
            }
          : "saved",
  }));
  const host = await import("./hostClient");
  const context = await host.nativeServerContext({ id: "remote" });
  const receive = vi.fn();
  const stop = await context.subscribe("entities", { after: "7" }, receive);
  const subscribe = mock
    .requests()
    .find(
      (request) =>
        request.method === "service.request" &&
        request.params.method === "view.subscribe",
    );
  const payload = {
    event: `owned:${subscribe.params.params.subscription}`,
    value: { revision: "8" },
  };
  mock.channels[0].onmessage({ event: "studio-client-event", payload });
  expect(receive).toHaveBeenCalledWith(payload);
  await stop();
  expect(mock.requests().at(-1)).toMatchObject({
    method: "service.request",
    params: {
      handle: "owned",
      method: "view.unsubscribe",
      params: { subscription: subscribe.params.params.subscription },
    },
  });
});
it("ignores malformed envelopes and rejects topics outside the fixed broker", async () => {
  const mock = transport();
  const host = await import("./hostClient");
  const callback = vi.fn();
  await host.listenNativeEvents(callback);
  for (const input of [
    null,
    [],
    { event: "studio-client-event" },
    { event: "unknown", payload: "bad" },
    { event: "studio-client-event", payload: "bad", extra: true },
  ])
    mock.channels[0].onmessage(input);
  expect(callback).not.toHaveBeenCalled();
  await expect(
    host.listenHostEvent("foreign" as any, callback),
  ).rejects.toThrow(/topic/);
  expect(
    mock.requests().filter((request) => request.method === "ui.events.connect"),
  ).toHaveLength(1);
});
it("keeps the document connection when one watcher stops and disconnects only at explicit dispose", async () => {
  const mock = transport();
  const host = await import("./hostClient");
  const first = vi.fn();
  const second = vi.fn();
  const stop = await host.listenNativeEvents(first);
  await host.listenNativeEvents(second);
  stop();
  stop();
  mock.channels[0].onmessage({ event: "studio-client-event", payload: "view" });
  expect(first).not.toHaveBeenCalled();
  expect(second).toHaveBeenCalledWith("view");
  expect(
    mock
      .requests()
      .some((request) => request.method === "ui.events.disconnect"),
  ).toBe(false);
  await host.disposeHostClient();
  await host.disposeHostClient();
  mock.channels[0].onmessage({ event: "studio-client-event", payload: "late" });
  expect(second).toHaveBeenCalledTimes(1);
  expect(
    mock
      .requests()
      .filter((request) => request.method === "ui.events.disconnect"),
  ).toMatchObject([{ params: { leaseId: "exact-lease" } }]);
  expect(
    mock
      .requests()
      .some((request) => /cancel|view.unsubscribe/.test(request.method)),
  ).toBe(false);
});
it("waits for a pending connection lease before one exact disconnect", async () => {
  let finish!: (value: unknown) => void;
  const mock = transport(({ id, method }) =>
    method === "ui.events.connect"
      ? new Promise((resolve) => {
          finish = (result) => resolve({ id, result });
        })
      : { id, result: { disconnected: true } },
  );
  const host = await import("./hostClient");
  const callback = vi.fn();
  const listening = host.listenNativeEvents(callback);
  await vi.waitFor(() => expect(finish).toBeTypeOf("function"));
  const disposing = host.disposeHostClient();
  mock.channels[0].onmessage({ event: "studio-client-event", payload: "late" });
  expect(callback).not.toHaveBeenCalled();
  const stopped = expect(listening).rejects.toThrow(/disposed/);
  finish({ leaseId: "late-exact-lease" });
  await stopped;
  await disposing;
  expect(
    mock
      .requests()
      .filter((request) => request.method === "ui.events.disconnect"),
  ).toMatchObject([{ params: { leaseId: "late-exact-lease" } }]);
});
it("holds an uncertain connection without reconnect or event fallback", async () => {
  const mock = transport(() => {
    throw new Error("connect outcome unknown");
  });
  const host = await import("./hostClient");
  await expect(host.listenNativeEvents(vi.fn())).rejects.toThrow(
    "connect outcome unknown",
  );
  await expect(host.listenNativeEvents(vi.fn())).rejects.toThrow(
    "connect outcome unknown",
  );
  await expect(host.disposeHostClient()).rejects.toThrow(
    "connect outcome unknown",
  );
  expect(
    mock.requests().filter((request) => request.method === "ui.events.connect"),
  ).toHaveLength(1);
  expect(mock.eventListen).not.toHaveBeenCalled();
});
it("does not repeat a disconnect after its response is lost", async () => {
  const mock = transport(({ id, method }) => {
    if (method === "ui.events.disconnect")
      throw new Error("disconnect outcome unknown");
    return { id, result: { leaseId: "owned-lease" } };
  });
  const host = await import("./hostClient");
  await host.connectHostEvents();
  await expect(host.disposeHostClient()).rejects.toThrow(
    "disconnect outcome unknown",
  );
  await expect(host.disposeHostClient()).rejects.toThrow(
    "disconnect outcome unknown",
  );
  await expect(host.connectHostEvents()).rejects.toThrow(/disposed/);
  expect(
    mock
      .requests()
      .filter((request) => request.method === "ui.events.disconnect"),
  ).toHaveLength(1);
});
it.each([
  null,
  { leaseId: "" },
  { leaseId: 7 },
  { leaseId: "owned", extra: "foreign" },
])(
  "rejects malformed connect leases without a second connection: %j",
  async (result) => {
    const mock = transport(({ id }) => ({ id, result }));
    const host = await import("./hostClient");
    await expect(host.connectHostEvents()).rejects.toThrow(/lease/);
    await expect(host.connectHostEvents()).rejects.toThrow(/lease/);
    expect(
      mock
        .requests()
        .filter((request) => request.method === "ui.events.connect"),
    ).toHaveLength(1);
  },
);
it("keeps pending business commands unchanged when UI watchers are disposed", async () => {
  let finish!: () => void;
  const mock = transport(
    ({ id }) =>
      new Promise((resolve) => {
        finish = () => resolve({ id, result: "committed" });
      }),
  );
  const host = await import("./hostClient");
  const write = host.nativeRequest(
    "outbox.enqueue",
    { body: "original" },
    "write-id",
  );
  await host.disposeHostClient();
  expect(mock.requests()).toEqual([
    { id: "write-id", method: "outbox.enqueue", params: { body: "original" } },
  ]);
  finish();
  await expect(write).resolves.toBe("committed");
});
it("finishes fanout before surfacing a watcher exception", async () => {
  const mock = transport();
  const host = await import("./hostClient");
  const second = vi.fn();
  await host.listenNativeEvents(() => {
    throw new Error("view failed");
  });
  await host.listenNativeEvents(second);
  expect(() =>
    mock.channels[0].onmessage({
      event: "studio-client-event",
      payload: "view",
    }),
  ).toThrow("view failed");
  expect(second).toHaveBeenCalledWith("view");
});
it("requires the private main document and never uses a frame or browser fallback", async () => {
  const mock = transport();
  const host = await import("./hostClient");
  mock.current.top = {};
  expect(host.hasNativeClient()).toBe(false);
  await expect(host.nativeRequest("read", {}, "id")).rejects.toThrow(
    /unavailable/,
  );
  mock.current.top = mock.current;
  mock.current.location.protocol = "studio-view:";
  expect(host.hasNativeClient()).toBe(false);
  await expect(host.connectHostEvents()).rejects.toThrow(/unavailable/);
  expect(mock.invoke).not.toHaveBeenCalled();
  expect(mock.eventListen).not.toHaveBeenCalled();
});
it("matches the Rust broker owner and the pinned global host configuration", async () => {
  const host = await import("./hostClient");
  const source = await readFile(
    fileURLToPath(
      new URL("../../../native/src-tauri/src/ui_events.rs", import.meta.url),
    ),
    "utf8",
  );
  const topics = source
    .match(/const TOPICS:[^=]+\[([\s\S]*?)\];/)?.[1]
    .match(/"[^"]+"/g)
    ?.map((value) => JSON.parse(value));
  expect(topics).toEqual([...host.HOST_EVENT_TOPICS]);
  expect(source).toContain(
    `const CONNECT_METHOD: &str = "${host.HOST_EVENT_CONNECT_METHOD}"`,
  );
  expect(source).toContain(
    `const DISCONNECT_METHOD: &str = "${host.HOST_EVENT_DISCONNECT_METHOD}"`,
  );
  const config = JSON.parse(
    await readFile(
      fileURLToPath(
        new URL("../../../native/src-tauri/tauri.conf.json", import.meta.url),
      ),
      "utf8",
    ),
  );
  expect(config.app.withGlobalTauri).toBe(true);
  const manifest = await readFile(
    fileURLToPath(
      new URL("../../../native/src-tauri/Cargo.toml", import.meta.url),
    ),
    "utf8",
  );
  expect(manifest).toContain('tauri = { version = "=2.12.5"');
});
it("keeps separate watcher lifetimes when callers share the same callback", async () => {
  const mock = transport();
  const host = await import("./hostClient");
  const callback = vi.fn();
  const first = await host.listenNativeEvents(callback);
  await host.listenNativeEvents(callback);
  mock.channels[0].onmessage({
    event: "studio-client-event",
    payload: "first",
  });
  expect(callback).toHaveBeenCalledTimes(2);
  first();
  mock.channels[0].onmessage({
    event: "studio-client-event",
    payload: "second",
  });
  expect(callback).toHaveBeenCalledTimes(3);
});
it("does not start a view subscription when its pending UI connection is disposed", async () => {
  let finish!: () => void;
  const mock = transport(({ id, method, params }) => {
    if (method === "service.open")
      return {
        id,
        result: {
          handle: "owned",
          serverId: params.serverId,
          workspaceId: "a".repeat(32),
          origin: "https://owned.invalid",
          credentialId: null,
        },
      };
    if (method === "ui.events.connect")
      return new Promise((resolve) => {
        finish = () => resolve({ id, result: { leaseId: "late-lease" } });
      });
    return { id, result: { disconnected: true } };
  });
  const host = await import("./hostClient");
  const context = await host.nativeServerContext({ id: "remote" });
  const pending = context.subscribe("entities", {}, vi.fn());
  await vi.waitFor(() => expect(finish).toBeTypeOf("function"));
  const disposed = host.disposeHostClient();
  finish();
  await expect(pending).rejects.toThrow(/disposed/);
  await disposed;
  expect(
    mock
      .requests()
      .some((request) => request.params?.method === "view.subscribe"),
  ).toBe(false);
});
