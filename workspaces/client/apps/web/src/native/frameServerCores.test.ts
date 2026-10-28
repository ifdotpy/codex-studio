import { afterEach, expect, it, vi } from "vitest";
import type { CoreClient } from "./client";
import { bindNativeFrames } from "./frame";

afterEach(() => vi.unstubAllGlobals());

type Listener = (event: MessageEvent) => Promise<void>;

function fixture() {
  let listener: Listener | undefined;
  vi.stubGlobal("window", {
    addEventListener: (_name: string, callback: Listener) => {
      listener = callback;
    },
    removeEventListener: vi.fn(),
  });
  const frame = (owner: string, host: string) => ({
    src: `studio-view://${host}/?studio-server=${owner}`,
    isConnected: true,
    contentWindow: { postMessage: vi.fn() },
  });
  const views = {
    a: frame("server-a", "studio-a.localhost"),
    b: frame("server-b", "studio-b.localhost"),
  };
  const frames = new Map([
    ["server-a", views.a as unknown as HTMLIFrameElement],
    ["server-b", views.b as unknown as HTMLIFrameElement],
  ]);
  const emit = new Map<string, (value: unknown) => void>();
  const cores = new Map<
    string,
    { core: CoreClient; request: ReturnType<typeof vi.fn> }
  >();
  const coreFor = (server: string): CoreClient => {
    let fake = cores.get(server);
    if (!fake) {
      const request = vi.fn(async (..._args: unknown[]) => ({ server }));
      fake = {
        request,
        core: {
          server,
          request: <T>(method: string, params: unknown, id?: string) =>
            request(method, params, id) as Promise<T>,
          listen: async (callback: (value: unknown) => void) => {
            emit.set(server, callback);
            return () => emit.delete(server);
          },
        },
      };
      cores.set(server, fake);
    }
    return fake.core;
  };
  const invoke = vi.fn(async () => "local");
  const dispose = bindNativeFrames(frames, invoke, undefined, coreFor);
  const send = (
    view: keyof typeof views,
    id: string,
    method: string,
    params: unknown,
  ) =>
    listener!({
      source: views[view].contentWindow,
      origin: `studio-view://studio-${view}.localhost`,
      data: { kind: "studio-native-frame-request", id, method, params },
    } as unknown as MessageEvent);
  const reply = (view: keyof typeof views) =>
    views[view].contentWindow.postMessage.mock.lastCall?.[0];
  return { views, cores, emit, invoke, dispose, send, reply };
}

it("sends a remote view's outbox and drafts to that server's core, not the local one", async () => {
  const { cores, invoke, send, reply, dispose } = fixture();
  await send("a", "list", "outbox.list", { workspace: "w" });
  await send("a", "edit", "drafts.edit", { workspace: "w" });
  await send("a", "upload", "uploads.enqueue", { workspace: "w" });
  expect(invoke).not.toHaveBeenCalled();
  const core = cores.get("server-a")!;
  expect(core.request).toHaveBeenCalledWith(
    "outbox.list",
    { workspace: "w" },
    "list",
  );
  expect(core.request).toHaveBeenCalledWith(
    "drafts.edit",
    { workspace: "w" },
    "edit",
  );
  expect(core.request).toHaveBeenCalledWith(
    "uploads.enqueue",
    { workspace: "w" },
    "upload",
  );
  expect(reply("a")).toEqual({
    kind: "studio-native-frame-response",
    id: "upload",
    result: { server: "server-a" },
  });
  dispose();
});

it("never lets a request of server A reach the core of server B", async () => {
  const { cores, send, emit, views, dispose } = fixture();
  await send("a", "one", "api.request", { path: "/api/sync/identity" });
  await send("a", "view", "view.subscribe", {
    subscription: "s",
    topic: "outbox",
  });
  expect(cores.get("server-a")?.request).toHaveBeenCalledTimes(2);
  expect(cores.get("server-b")).toBeUndefined();
  // The events of server A's core reach only the view of server A.
  emit.get("server-a")!({ event: "s", value: 1 });
  expect(views.a.contentWindow.postMessage).toHaveBeenLastCalledWith(
    { kind: "studio-native-core-event", value: { event: "s", value: 1 } },
    "studio-view://studio-a.localhost",
  );
  expect(views.b.contentWindow.postMessage).not.toHaveBeenCalled();
  dispose();
});

it("still refuses unknown methods, device topics and server management", async () => {
  const { cores, invoke, send, reply, dispose } = fixture();
  for (const [method, params] of [
    ["secrets.read", {}],
    ["preferences.raw_set", { key: "other-store" }],
    ["servers.request", { serverId: "server-b" }],
    ["view.subscribe", { subscription: "n", topic: "navigation" }],
    ["dictation.wav", { id: "r" }],
    ["api.request", { path: "/api/multi-server", method: "POST", body: {} }],
    ["api.request", { path: "/api/multi-server/v1/status" }],
  ] as const) {
    await send("a", method, method, params);
    expect(reply("a"), method).toMatchObject({
      id: method,
      error: expect.any(String),
    });
  }
  expect(invoke).not.toHaveBeenCalled();
  expect(cores.get("server-a")?.request ?? vi.fn()).not.toHaveBeenCalled();
  dispose();
});
