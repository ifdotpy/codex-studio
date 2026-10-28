import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

class FakeEventSource extends EventTarget {
  static opened: FakeEventSource[] = [];
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
  closed = false;
  readonly url: string;
  constructor(url: string) {
    super();
    this.url = url;
    FakeEventSource.opened.push(this);
  }
  close() {
    this.closed = true;
  }
  emit(value: unknown) {
    this.onmessage?.(
      new MessageEvent("message", { data: JSON.stringify(value) }),
    );
  }
}

type Sent = { url: string; init: RequestInit | undefined };

function server(
  reply: (body: { id: string; method: string; params: unknown }) => unknown,
) {
  const sent: Sent[] = [];
  const fetch = vi.fn(async (url: string, init?: RequestInit) => {
    sent.push({ url, init });
    if (url === "/api/session") return Response.json({ token: "token" });
    const body = JSON.parse(String(init?.body));
    return Response.json(await reply(body));
  });
  vi.stubGlobal("fetch", fetch);
  return sent;
}

async function load() {
  vi.resetModules();
  return import("./client");
}

beforeEach(() => {
  FakeEventSource.opened = [];
  vi.stubGlobal("location", { protocol: "https:" });
  vi.stubGlobal("EventSource", FakeEventSource);
});
afterEach(() => vi.unstubAllGlobals());

describe("hosted client transport", () => {
  it("serves a plain browser page without device commands", async () => {
    server(({ id }) => ({ id, result: null }));
    const client = await load();
    expect(client.hasNativeClient()).toBe(true);
    expect(client.hasDeviceClient()).toBe(false);
  });

  it("sends the exact request identity with the session token", async () => {
    const sent = server(({ id, params }) => ({ id, result: params }));
    const { nativeRequest } = await load();
    await expect(
      nativeRequest("preferences.raw_set", { key: "a" }, "exact-id"),
    ).resolves.toEqual({ key: "a" });
    const request = sent.find((entry) => entry.url === "/api/client/request");
    expect(JSON.parse(String(request?.init?.body))).toEqual({
      id: "exact-id",
      method: "preferences.raw_set",
      params: { key: "a" },
    });
    expect(request?.init?.method).toBe("POST");
    expect(request?.init?.headers).toMatchObject({ "x-canvas-token": "token" });
  });

  it("reports a lost response as an unknown outcome and never repeats it", async () => {
    const sent: Sent[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        sent.push({ url, init });
        if (url === "/api/session") return Response.json({ token: "token" });
        throw new TypeError("network connection lost");
      }),
    );
    const { nativeRequest, OutcomeUnknownError } = await load();
    const result = nativeRequest("outbox.enqueue", { text: "hi" }, "lost-id");
    await expect(result).rejects.toBeInstanceOf(OutcomeUnknownError);
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(
      sent.filter((entry) => entry.url === "/api/client/request"),
    ).toHaveLength(1);
  });

  it("rejects a reply for another request identity", async () => {
    server(() => ({ id: "other", result: true }));
    const { nativeRequest } = await load();
    await expect(nativeRequest("system.info", null, "mine")).rejects.toThrow(
      "mismatched response",
    );
  });

  it("returns the core error of a refused device command", async () => {
    server(({ id }) => ({
      id,
      error: {
        code: "unsupported_here",
        message: "This action needs your device.",
      },
    }));
    const { nativeRequest } = await load();
    await expect(nativeRequest("media.captureStart", null)).rejects.toThrow(
      "This action needs your device.",
    );
  });

  it("waits for the first reply before a second request, so one device cookie is made", async () => {
    let release: (() => void) | undefined;
    const order: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        if (url === "/api/session") return Response.json({ token: "token" });
        const { id } = JSON.parse(String(init?.body));
        order.push(`sent ${id}`);
        if (id === "first")
          await new Promise<void>((resolve) => (release = resolve));
        return Response.json({ id, result: id });
      }),
    );
    const { nativeRequest } = await load();
    const first = nativeRequest("system.info", null, "first");
    const second = nativeRequest("system.info", null, "second");
    await vi.waitFor(() => expect(order).toEqual(["sent first"]));
    release?.();
    await expect(Promise.all([first, second])).resolves.toEqual([
      "first",
      "second",
    ]);
    expect(order).toEqual(["sent first", "sent second"]);
  });

  it("shares one event stream between listeners and closes it after the last", async () => {
    server(({ id }) => ({ id, result: null }));
    const { listenNativeEvents } = await load();
    const first = vi.fn();
    const second = vi.fn();
    const opening = listenNativeEvents(first);
    await vi.waitFor(() => expect(FakeEventSource.opened).toHaveLength(1));
    const stream = FakeEventSource.opened[0]!;
    expect(stream.url).toBe("/api/client/events");
    stream.dispatchEvent(new Event("open"));
    const stopFirst = await opening;
    const stopSecond = await listenNativeEvents(second);
    stream.emit({ event: "view", value: 1 });
    expect(first).toHaveBeenCalledWith({ event: "view", value: 1 });
    expect(second).toHaveBeenCalledWith({ event: "view", value: 1 });
    stopFirst();
    expect(stream.closed).toBe(false);
    stopSecond();
    expect(stream.closed).toBe(true);
    expect(FakeEventSource.opened).toHaveLength(1);
  });
});
