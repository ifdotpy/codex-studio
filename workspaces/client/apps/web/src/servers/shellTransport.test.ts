import { afterEach, expect, it, vi } from "vitest";
import { bindShellTransport } from "./shellTransport";
vi.mock("./registry", () => ({
  readServers: () => [
    { id: "remote", origin: "https://remote.tailnet.ts.net" },
  ],
}));
vi.mock("./transport", () => ({
  serverCredentialAdapter: () => ({
    fetch: (_server: unknown, request: Request) => fetch(request),
  }),
}));
afterEach(() => vi.unstubAllGlobals());
it("keeps the original stream tracked after replay and aborts it on frame removal", async () => {
  let receive!: (event: unknown) => Promise<void>, removed!: () => void;
  let signal!: AbortSignal;
  const replies: Record<string, unknown>[] = [];
  const frame = {
    src: "http://studio-remote.localhost:1234/",
    isConnected: true,
    contentWindow: {
      postMessage: (value: unknown) =>
        replies.push(value as Record<string, unknown>),
    },
  };
  vi.stubGlobal("window", {
    addEventListener: (_: string, fn: typeof receive) => (receive = fn),
    removeEventListener() {},
  });
  vi.stubGlobal("document", { body: {} });
  vi.stubGlobal(
    "MutationObserver",
    class {
      constructor(fn: () => void) {
        removed = fn;
      }
      observe() {}
      disconnect() {}
    },
  );
  vi.stubGlobal("fetch", async (request: Request) => {
    signal = request.signal;
    return new Response(
      new ReadableStream({
        start(controller) {
          signal.addEventListener("abort", () => controller.close());
        },
      }),
      { headers: { "Content-Type": "text/event-stream" } },
    );
  });
  const stop = bindShellTransport(
    new Map([["remote", frame as unknown as HTMLIFrameElement]]),
  );
  const event = {
    source: frame.contentWindow,
    origin: new URL(frame.src).origin,
    data: {
      kind: "studio-server-transport",
      correlation: "first",
      value: {
        action: "request",
        serverId: "remote",
        streamId: "same",
        url: "https://remote.tailnet.ts.net/api/sync/stream",
        method: "GET",
      },
    },
  };
  const original = receive(event);
  await vi.waitFor(() => expect(replies).toHaveLength(1));
  await receive({ ...event, data: { ...event.data, correlation: "replay" } });
  expect(replies[1].error).toBe("Invalid server transport action.");
  expect(signal.aborted).toBe(false);
  frame.isConnected = false;
  removed();
  expect(signal.aborted).toBe(true);
  await original;
  stop();
});

it("rejects management writes and pairing routes from an owned frame", async () => {
  let receive!: (event: unknown) => Promise<void>;
  const replies: Record<string, unknown>[] = [];
  const fetchRequest = vi.fn(async () => new Response("ok"));
  const frame = {
    src: "http://studio-remote.localhost:1234/",
    isConnected: true,
    contentWindow: {
      postMessage: (value: unknown) =>
        replies.push(value as Record<string, unknown>),
    },
  };
  vi.stubGlobal("window", {
    addEventListener: (_: string, fn: typeof receive) => (receive = fn),
    removeEventListener() {},
  });
  vi.stubGlobal("document", { body: {} });
  vi.stubGlobal(
    "MutationObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  vi.stubGlobal("fetch", fetchRequest);
  const stop = bindShellTransport(
    new Map([["remote", frame as unknown as HTMLIFrameElement]]),
  );
  for (const path of [
    "/api/multi-server",
    "/api/multi-server/",
    "/api/multi%2dserver",
    "/api/multi-server/v1/pair",
    "/api/multi-server/v1/auto-pair",
  ]) {
    await receive({
      source: frame.contentWindow,
      origin: new URL(frame.src).origin,
      data: {
        kind: "studio-server-transport",
        correlation: path,
        value: {
          action: "request",
          serverId: "remote",
          streamId: path,
          url: "https://remote.tailnet.ts.net" + path,
          method: "POST",
          body: new TextEncoder().encode('{"action":"unrevoke"}').buffer,
        },
      },
    });
    expect(replies.at(-1)?.error).toBe(
      "Use the workspace shell for server management.",
    );
  }
  expect(fetchRequest).not.toHaveBeenCalled();
  await receive({
    source: frame.contentWindow,
    origin: new URL(frame.src).origin,
    data: {
      kind: "studio-server-transport",
      correlation: "message",
      value: {
        action: "request",
        serverId: "remote",
        streamId: "message",
        url: "https://remote.tailnet.ts.net/api/messages",
        method: "POST",
      },
    },
  });
  expect(fetchRequest).toHaveBeenCalledOnce();
  stop();
});
