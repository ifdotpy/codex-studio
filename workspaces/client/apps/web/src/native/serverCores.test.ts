import { afterEach, describe, expect, it, vi } from "vitest";

class FakeEventSource extends EventTarget {
  static opened: FakeEventSource[] = [];
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
  readonly url: string;
  constructor(url: string) {
    super();
    this.url = url;
    FakeEventSource.opened.push(this);
  }
  close() {}
  emit(value: unknown) {
    this.onmessage?.(
      new MessageEvent("message", { data: JSON.stringify(value) }),
    );
  }
}

async function load() {
  vi.resetModules();
  return import("./client");
}

afterEach(() => vi.unstubAllGlobals());

describe("one client core per server in the desktop app", () => {
  function desktop() {
    let deliver: ((event: { payload: unknown }) => void)[] = [];
    const invoke = vi.fn(
      async (_command: string, args: Record<string, unknown>) => {
        const request = args.request as { id: string };
        return { id: request.id, result: args.server ?? "local" };
      },
    );
    vi.stubGlobal("location", { protocol: "tauri:", search: "" });
    vi.stubGlobal("__TAURI__", {
      core: { invoke },
      event: {
        listen: async (
          _name: string,
          callback: (event: { payload: unknown }) => void,
        ) => {
          deliver.push(callback);
          return () => {
            deliver = deliver.filter((value) => value !== callback);
          };
        },
      },
    });
    return {
      invoke,
      emit: (payload: unknown) => deliver.forEach((send) => send({ payload })),
    };
  }

  it("names the server of each request and keeps local requests unnamed", async () => {
    const { invoke } = desktop();
    const { coreForServer, localCore } = await load();
    await expect(
      coreForServer("server-a").request("outbox.list", { w: 1 }, "exact"),
    ).resolves.toBe("server-a");
    expect(invoke).toHaveBeenLastCalledWith("client_request", {
      request: { id: "exact", method: "outbox.list", params: { w: 1 } },
      server: "server-a",
    });
    await expect(
      localCore.request("outbox.list", {}, "local-id"),
    ).resolves.toBe("local");
    expect(invoke.mock.lastCall?.[1]).not.toHaveProperty("server");
    expect(coreForServer("local")).toBe(localCore);
  });

  it("delivers each core's events only to the listeners of that server", async () => {
    const { emit } = desktop();
    const { coreForServer, localCore } = await load();
    const local = vi.fn();
    const a = vi.fn();
    const b = vi.fn();
    await localCore.listen(local);
    await coreForServer("server-a").listen(a);
    await coreForServer("server-b").listen(b);
    emit({ event: "x", server: "server-a" });
    emit({ event: "y" });
    expect(a).toHaveBeenCalledWith({ event: "x", server: "server-a" });
    expect(b).not.toHaveBeenCalled();
    expect(local).toHaveBeenCalledTimes(1);
    expect(local).toHaveBeenCalledWith({ event: "y" });
  });
});

describe("one client core per server in a browser page", () => {
  it("asks the page's server for the core of a paired server", async () => {
    FakeEventSource.opened = [];
    vi.stubGlobal("location", { protocol: "https:", search: "" });
    vi.stubGlobal("EventSource", FakeEventSource);
    const urls: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        urls.push(url);
        if (url === "/api/session") return Response.json({ token: "token" });
        const body = JSON.parse(String(init?.body));
        return Response.json({ id: body.id, result: url });
      }),
    );
    const { coreForServer, localCore } = await load();
    await expect(
      coreForServer("server-a").request("drafts.view", {}, "a-id"),
    ).resolves.toBe("/api/client/request?server=server-a");
    await expect(localCore.request("drafts.view", {}, "own-id")).resolves.toBe(
      "/api/client/request",
    );
    void coreForServer("server-a").listen(vi.fn());
    await vi.waitFor(() =>
      expect(FakeEventSource.opened.map((source) => source.url)).toContain(
        "/api/client/events?server=server-a",
      ),
    );
  });
});
