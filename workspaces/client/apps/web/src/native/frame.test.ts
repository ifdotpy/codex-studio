import { afterEach, expect, it, vi } from "vitest";
import { bindNativeFrames, frameRequest } from "./frame";

it("gets an identity from Rust without a browser timer or crypto", async () => {
  const listeners = new Set<(event: MessageEvent) => void>();
  const timer = vi.fn(() => {
    throw new Error("Browser deadlines are forbidden.");
  });
  const parent = {
    postMessage: vi.fn((message: any) =>
      queueMicrotask(() => {
        for (const listener of listeners)
          listener({
            source: parent,
            origin: "tauri://localhost",
            data: {
              kind: "studio-native-frame-response",
              id: message.id,
              result:
                message.method === "system.uuid"
                  ? "00000000-0000-4000-8000-000000000001"
                  : true,
            },
          } as unknown as MessageEvent);
      }),
    ),
  };
  vi.stubGlobal("crypto", undefined);
  vi.stubGlobal("window", {
    parent,
    setTimeout: timer,
    addEventListener: (name: string, callback: any) => {
      if (name === "message") listeners.add(callback);
    },
    removeEventListener: (name: string, callback: any) => {
      if (name === "message") listeners.delete(callback);
    },
  });
  await expect(frameRequest("read", {})).resolves.toBe(true);
  expect(
    parent.postMessage.mock.calls.map(([request]) => request.method),
  ).toEqual(["system.uuid", "read"]);
  expect(parent.postMessage.mock.calls[1][0].id).toBe(
    "00000000-0000-4000-8000-000000000001",
  );
  expect(timer).not.toHaveBeenCalled();
  expect(listeners.size).toBe(0);
});

afterEach(() => vi.unstubAllGlobals());
it("selects a native context from the frame owner and refuses foreign handles", async () => {
  let listener: (event: MessageEvent) => Promise<void>;
  vi.stubGlobal("window", {
    addEventListener: (_: string, callback: typeof listener) => {
      listener = callback;
    },
    removeEventListener: vi.fn(),
  });
  const source = { postMessage: vi.fn() };
  const frame = {
    src: "studio-view://studio-remote.localhost/?studio-server=remote",
    isConnected: true,
    contentWindow: source,
  };
  const otherSource = { postMessage: vi.fn() };
  const other = {
    src: "studio-view://studio-other.localhost/?studio-server=other",
    isConnected: true,
    contentWindow: otherSource,
  };
  const frames = new Map([
    ["remote", frame as unknown as HTMLIFrameElement],
    ["other", other as unknown as HTMLIFrameElement],
  ]);
  const invoke = vi.fn(async (method: string) =>
    method === "service.open"
      ? {
          handle: "remote-capability",
          serverId: "remote",
          workspaceId: "a".repeat(32),
        }
      : { saved: true },
  );
  const dispose = bindNativeFrames(frames, invoke);
  const event = (method: string, params: unknown, caller = frame) =>
    ({
      source: caller.contentWindow,
      origin: new URL(caller.src).origin,
      data: {
        kind: "studio-native-frame-request",
        id: "exact",
        method,
        params,
      },
    }) as unknown as MessageEvent;
  // Custom-scheme URL.origin is null in Node, so use the actual WebView origin.
  const from = (method: string, params: unknown, caller = frame) =>
    ({
      ...event(method, params, caller),
      origin: `studio-view://${new URL(caller.src).host}`,
    }) as MessageEvent;
  await listener!(
    from("service.open", {
      serverId: "local",
      stateDirectory: "/foreign",
      credentialId: "forged",
    }),
  );
  expect(invoke).toHaveBeenLastCalledWith(
    "service.open",
    { serverId: "remote" },
    "exact",
  );
  await listener!(
    from(
      "service.request",
      {
        handle: "remote-capability",
        method: "outbox.enqueue",
        params: { body: { id: "original" } },
      },
      other,
    ),
  );
  expect(otherSource.postMessage.mock.lastCall?.[0].error).toContain(
    "another server view",
  );
  expect(invoke).toHaveBeenCalledTimes(1);
  await listener!(
    from("service.request", {
      handle: "remote-capability",
      method: "outbox.enqueue",
      params: { body: { id: "original" } },
    }),
  );
  expect(invoke).toHaveBeenCalledTimes(2);
  frames.delete("remote");
  frame.src = "studio-view://studio-third.localhost/?studio-server=third";
  frames.set("third", frame as unknown as HTMLIFrameElement);
  await listener!(
    from("service.request", {
      handle: "remote-capability",
      method: "outbox.enqueue",
      params: {},
    }),
  );
  expect(source.postMessage.mock.lastCall?.[0].error).toContain(
    "another server view",
  );
  expect(invoke).toHaveBeenCalledTimes(2);
  dispose();
});
it("derives the caller from the live frame and exposes only pure commands", async () => {
  let listener: (event: MessageEvent) => Promise<void>;
  vi.stubGlobal("window", {
    addEventListener: (_name: string, callback: typeof listener) => {
      listener = callback;
    },
    removeEventListener: vi.fn(),
  });
  const source = { postMessage: vi.fn() };
  const frame = {
    src: "studio-view://studio-test.localhost/?studio-server=local",
    isConnected: true,
    contentWindow: source,
  };
  const frames = new Map([["local", frame as unknown as HTMLIFrameElement]]);
  const invoke = vi.fn(async () => ({ blocks: [] }));
  const dispose = bindNativeFrames(frames, invoke);
  const event = (changes = {}) =>
    ({
      source,
      origin: "studio-view://studio-test.localhost",
      data: {
        kind: "studio-native-frame-request",
        id: "one",
        method: "markdown.parse",
        params: { text: "hello" },
      },
      ...changes,
    }) as unknown as MessageEvent;
  await listener!(event({ source: {} }));
  await listener!(event({ origin: "studio-view://other.localhost" }));
  expect(invoke).not.toHaveBeenCalled();
  await listener!(event());
  expect(invoke).toHaveBeenCalledWith(
    "markdown.parse",
    { text: "hello" },
    "one",
  );
  expect(source.postMessage).toHaveBeenLastCalledWith(
    { kind: "studio-native-frame-response", id: "one", result: { blocks: [] } },
    "studio-view://studio-test.localhost",
  );
  await listener!(
    event({
      data: {
        kind: "studio-native-frame-request",
        id: "two",
        method: "preferences.raw_set",
        params: { key: "wrong-store" },
      },
    }),
  );
  expect(invoke).toHaveBeenCalledTimes(1);
  expect(source.postMessage.mock.lastCall?.[0]).toMatchObject({
    id: "two",
    error: expect.any(String),
  });
  frame.isConnected = false;
  await listener!(event());
  expect(invoke).toHaveBeenCalledTimes(1);
  dispose();
});

it("does not send a late result to a replaced server view", async () => {
  let listener: (event: MessageEvent) => Promise<void>;
  vi.stubGlobal("window", {
    addEventListener: (_name: string, callback: typeof listener) => {
      listener = callback;
    },
    removeEventListener: vi.fn(),
  });
  const source = { postMessage: vi.fn() };
  const frame = {
    src: "studio-view://studio-test.localhost/?studio-server=local",
    isConnected: true,
    contentWindow: source,
  };
  const frames = new Map([["local", frame as unknown as HTMLIFrameElement]]);
  let finish: (value: unknown) => void;
  const dispose = bindNativeFrames(
    frames,
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  const pending = listener!({
    source,
    origin: "studio-view://studio-test.localhost",
    data: {
      kind: "studio-native-frame-request",
      id: "one",
      method: "markdown.parse",
      params: {},
    },
  } as unknown as MessageEvent);
  frames.delete("local");
  finish!({ blocks: [] });
  await pending;
  expect(source.postMessage).not.toHaveBeenCalled();
  dispose();
});

it.each([
  "drafts.edit",
  "desktop.fileAction",
  "desktop.pickDirectory",
  "desktop.pickAttachments",
])(
  "allows local %s with its original identity but denies remote access",
  async (method) => {
    let listener: (event: MessageEvent) => Promise<void>;
    vi.stubGlobal("window", {
      addEventListener: (_name: string, callback: typeof listener) => {
        listener = callback;
      },
      removeEventListener: vi.fn(),
    });
    const local = { postMessage: vi.fn() };
    const remote = { postMessage: vi.fn() };
    const frames = new Map([
      [
        "local",
        {
          src: "studio-view://studio-local.localhost/?studio-server=local",
          isConnected: true,
          contentWindow: local,
        },
      ],
      [
        "remote",
        {
          src: "studio-view://studio-remote.localhost/?studio-server=remote",
          isConnected: true,
          contentWindow: remote,
        },
      ],
    ]) as unknown as Map<string, HTMLIFrameElement>;
    const invoke = vi.fn(async () => ({ revision: 1 }));
    const dispose = bindNativeFrames(frames, invoke);
    const params = { workspace: "workspace", changes: { room: "keep" } };
    for (const [owner, source] of [
      ["local", local],
      ["remote", remote],
    ] as const) {
      await listener!({
        source,
        origin: `studio-view://studio-${owner}.localhost`,
        data: {
          kind: "studio-native-frame-request",
          id: "stable-edit",
          method,
          params,
        },
      } as unknown as MessageEvent);
    }
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith(method, params, "stable-edit");
    expect(remote.postMessage.mock.lastCall?.[0]).toMatchObject({
      id: "stable-edit",
      error: expect.any(String),
    });
    dispose();
  },
);

it("scopes browser preferences to the authenticated frame, ignoring caller scope fields", async () => {
  let listener: (event: MessageEvent) => Promise<void>;
  vi.stubGlobal("window", {
    addEventListener: (_name: string, callback: typeof listener) => {
      listener = callback;
    },
    removeEventListener: vi.fn(),
  });
  const source = { postMessage: vi.fn() };
  const frames = new Map([
    [
      "remote",
      {
        src: "studio-view://studio-remote.localhost/?studio-server=remote",
        isConnected: true,
        contentWindow: source,
      },
    ],
  ]) as unknown as Map<string, HTMLIFrameElement>;
  const invoke = vi.fn(async () => ({
    'browser:"root":theme': "root-secret",
    'browser:"server:local":theme': "local-secret",
    'browser:"server:remote":theme': "remote-value",
  }));
  const dispose = bindNativeFrames(frames, invoke);
  await listener!({
    source,
    origin: "studio-view://studio-remote.localhost",
    data: {
      kind: "studio-native-frame-request",
      id: "read",
      method: "browser.preferences",
      params: { method: "preferences.raw_all", scope: "root", params: {} },
    },
  } as unknown as MessageEvent);
  expect(source.postMessage.mock.lastCall?.[0]).toEqual({
    kind: "studio-native-frame-response",
    id: "read",
    result: { theme: "remote-value" },
  });
  await listener!({
    source,
    origin: "studio-view://studio-remote.localhost",
    data: {
      kind: "studio-native-frame-request",
      id: "write",
      method: "browser.preferences",
      params: {
        method: "preferences.raw_set",
        scope: "root",
        params: { key: "theme", raw: "new" },
      },
    },
  } as unknown as MessageEvent);
  expect(invoke).toHaveBeenLastCalledWith(
    "preferences.raw_set",
    {
      key: 'browser:"server:remote":theme',
      raw: "new",
    },
    "write",
  );
  dispose();
});

it("selects profile snapshots by the authenticated frame owner", async () => {
  let listener: (event: MessageEvent) => Promise<void>;
  vi.stubGlobal("window", {
    addEventListener: (_name: string, callback: typeof listener) => {
      listener = callback;
    },
    removeEventListener: vi.fn(),
  });
  const source = { postMessage: vi.fn() };
  const frame = {
    src: "studio-view://studio-remote.localhost/?studio-server=remote",
    isConnected: true,
    contentWindow: source,
  };
  const invoke = vi.fn(async () => null);
  const dispose = bindNativeFrames(
    new Map([["remote", frame as unknown as HTMLIFrameElement]]),
    invoke,
  );
  await listener!({
    source,
    origin: "studio-view://studio-remote.localhost",
    data: {
      kind: "studio-native-frame-request",
      id: "archive",
      method: "browser.profile",
      params: { owner: "root" },
    },
  } as unknown as MessageEvent);
  expect(invoke).toHaveBeenCalledWith(
    "profile.snapshot",
    { owner: "server:remote" },
    "archive",
  );
  dispose();
});

it("keeps save transfers bound to the originating frame", async () => {
  let listener!: (event: MessageEvent) => Promise<void>;
  vi.stubGlobal("window", {
    addEventListener: (_: string, cb: typeof listener) => {
      listener = cb;
    },
    removeEventListener: vi.fn(),
  });
  const make = (id: string) => ({
    src: `studio-view://studio-${id}.localhost/?studio-server=${id}`,
    isConnected: true,
    contentWindow: { postMessage: vi.fn() },
  });
  const first = make("first"),
    second = make("second");
  const frames = new Map([
    ["first", first],
    ["second", second],
  ]) as unknown as Map<string, HTMLIFrameElement>;
  const invoke = vi.fn(async () => ({ transfer: "secret" }));
  const dispose = bindNativeFrames(frames, invoke);
  const send = (frame: typeof first, method: string) =>
    listener({
      source: frame.contentWindow,
      origin: `studio-view://${new URL(frame.src).host}`,
      data: {
        kind: "studio-native-frame-request",
        id: method,
        method,
        params: { value: { transfer: "secret" } },
      },
    } as unknown as MessageEvent);
  await send(first, "desktop.saveBegin");
  await send(second, "desktop.saveFinish");
  expect(invoke).toHaveBeenCalledTimes(1);
  expect(second.contentWindow.postMessage.mock.lastCall?.[0].error).toContain(
    "does not belong",
  );
  await send(first, "desktop.saveChunk");
  expect(invoke).toHaveBeenCalledTimes(2);
  dispose();
  expect(invoke).toHaveBeenLastCalledWith("desktop.saveCancel", {
    value: { transfer: "secret" },
  });
});

it("binds a sign-in forward to its remote frame and denies local vault keys", async () => {
  let listener: (event: MessageEvent) => Promise<void>;
  vi.stubGlobal("window", {
    addEventListener: (_name: string, callback: typeof listener) => {
      listener = callback;
    },
    removeEventListener: vi.fn(),
  });
  const source = { postMessage: vi.fn() };
  const frame = {
    src: "studio-view://studio-test.localhost/?studio-server=remote",
    isConnected: true,
    contentWindow: source,
  };
  const invoke = vi.fn(async () => null);
  const dispose = bindNativeFrames(
    new Map([["remote", frame as unknown as HTMLIFrameElement]]),
    invoke,
  );
  const send = (method: string, params: unknown) =>
    listener!({
      source,
      origin: "studio-view://studio-test.localhost",
      data: {
        kind: "studio-native-frame-request",
        id: "same-request",
        method,
        params,
      },
    } as unknown as MessageEvent);
  await send("desktop.openSignIn", {
    value: { url: "https://example.com", forward: { server: "other" } },
    signInOwner: "other",
  });
  await send("desktop.showVaultKeys", { value: "project" });
  expect(invoke).not.toHaveBeenCalled();
  const value = { url: "https://example.com", forward: { server: "remote" } };
  await send("desktop.openSignIn", { value, signInOwner: "forged" });
  expect(invoke).toHaveBeenCalledExactlyOnceWith(
    "desktop.openSignIn",
    { value, signInOwner: "remote" },
    "same-request",
  );
  invoke.mockClear();
  await send("browser.profileChunk", { owner: "root", sha256: "a".repeat(64) });
  expect(invoke).toHaveBeenCalledWith(
    "profile.chunk",
    {
      owner: "server:remote",
      sha256: "a".repeat(64),
    },
    "same-request",
  );
  dispose();
});
