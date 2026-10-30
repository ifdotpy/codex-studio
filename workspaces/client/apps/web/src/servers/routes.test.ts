import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ROUTE_FAILURE_COOLDOWN_MS,
  RouteIdentityError,
  ServerRoutes,
  retarget,
  sendRouted,
} from "./routes";

const PAIRED = "https://mac.tail.ts.net";
const LAN = "https://192.168.1.5:8443";

/** A fake network: each origin answers with a server id, or fails. */
function network(answers: Record<string, string | "down">) {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  const request = vi.fn(
    async (url: string | URL | Request, init?: RequestInit) => {
      const href = String(url);
      calls.push({ url: href, ...(init ? { init } : {}) });
      const answer = answers[new URL(href).origin];
      if (!answer || answer === "down") throw new TypeError("network down");
      return Response.json({ serverId: answer });
    },
  );
  return { answers, calls, request: request as unknown as typeof fetch };
}

function routesWith(net: ReturnType<typeof network>, learned: string[] = []) {
  const saved: string[][] = [];
  const routes = new ServerRoutes(
    PAIRED,
    "srv",
    { read: () => learned, write: (value) => saved.push(value) },
    net.request,
  );
  return { routes, saved };
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(1_000_000);
});
afterEach(() => vi.useRealTimers());

describe("route list (rule 1)", () => {
  it("lists the paired origin first, then learned routes", () => {
    const { routes } = routesWith(network({}), [LAN]);
    expect(routes.routes()).toEqual([PAIRED, LAN]);
  });

  it("uses the first route that answers", async () => {
    const net = network({ [PAIRED]: "down", [LAN]: "srv" });
    const { routes } = routesWith(net, [LAN]);
    expect(await routes.resolve()).toBe(LAN);
    expect(net.calls.map((call) => new URL(call.url).origin)).toEqual([
      PAIRED,
      LAN,
    ]);
  });

  it("stays on the first route and asks once", async () => {
    const net = network({ [PAIRED]: "srv", [LAN]: "srv" });
    const { routes } = routesWith(net, [LAN]);
    await routes.resolve();
    await routes.resolve();
    expect(net.calls).toHaveLength(1);
  });
});

describe("identity before credentials (rule 2)", () => {
  it("asks without credentials and rejects another server", async () => {
    const net = network({ [PAIRED]: "other" });
    const { routes } = routesWith(net);
    await expect(routes.resolve()).rejects.toBeInstanceOf(RouteIdentityError);
    expect(net.calls[0]!.url).toBe(`${PAIRED}/api/multi-server/v1/identity`);
    expect(net.calls[0]!.init?.credentials).toBe("omit");
    expect(net.calls[0]!.init?.headers).toBeUndefined();
  });

  it("skips a route that now belongs to another machine", async () => {
    const net = network({ [PAIRED]: "down", [LAN]: "other" });
    const { routes } = routesWith(net, [LAN]);
    await expect(routes.resolve()).rejects.toBeInstanceOf(RouteIdentityError);
    expect(routes.current()).toBeUndefined();
  });
});

describe("learned routes (rule 3)", () => {
  it("adds reported origins and drops those no longer reported", () => {
    const { routes, saved } = routesWith(network({}));
    routes.learn([LAN, "https://pc.tail.ts.net"]);
    expect(routes.routes()).toEqual([PAIRED, LAN, "https://pc.tail.ts.net"]);
    routes.learn([LAN]);
    expect(routes.routes()).toEqual([PAIRED, LAN]);
    expect(saved.at(-1)).toEqual([LAN]);
  });

  it("ignores values that are not origins and the paired origin", () => {
    const { routes } = routesWith(network({}));
    routes.learn([PAIRED, "not a url", 5, "ftp://x", "https://u:p@h"]);
    expect(routes.routes()).toEqual([PAIRED]);
  });

  it("never sends a key paired over HTTPS over plain HTTP", async () => {
    const net = network({ [PAIRED]: "down", "http://192.168.1.5:8080": "srv" });
    const { routes } = routesWith(net);
    routes.learn(["http://192.168.1.5:8080"]);
    expect(routes.routes()).toEqual([PAIRED]);
    await expect(routes.resolve()).rejects.toBeInstanceOf(TypeError);
    expect(net.calls.every((call) => call.url.startsWith(PAIRED))).toBe(true);
  });
});

describe("return to the faster route (rule 4)", () => {
  async function onLan() {
    const net = network({ [PAIRED]: "down", [LAN]: "srv" });
    const { routes } = routesWith(net, [LAN]);
    expect(await routes.resolve()).toBe(LAN);
    const changed = vi.fn();
    routes.onChange(changed);
    return { net, routes, changed };
  }

  it("moves back when an earlier route works and tells the stream", async () => {
    const { net, routes, changed } = await onLan();
    net.answers[PAIRED] = "srv";
    vi.advanceTimersByTime(ROUTE_FAILURE_COOLDOWN_MS);
    await routes.returnToFaster();
    expect(routes.current()).toBe(PAIRED);
    expect(changed).toHaveBeenCalledWith(PAIRED);
  });

  it("keeps the route when no earlier route answers", async () => {
    const { routes, changed } = await onLan();
    vi.advanceTimersByTime(ROUTE_FAILURE_COOLDOWN_MS);
    await routes.returnToFaster();
    expect(routes.current()).toBe(LAN);
    expect(changed).not.toHaveBeenCalled();
  });

  it("waits a cooldown before it tries a failed route again", async () => {
    const { net, routes } = await onLan();
    const before = net.calls.length;
    net.answers[PAIRED] = "srv";
    await routes.returnToFaster();
    expect(net.calls).toHaveLength(before);
    expect(routes.current()).toBe(LAN);
    vi.advanceTimersByTime(ROUTE_FAILURE_COOLDOWN_MS);
    await routes.returnToFaster();
    expect(routes.current()).toBe(PAIRED);
  });

  it("does not check anything while the first route is in use", async () => {
    const net = network({ [PAIRED]: "srv" });
    const { routes } = routesWith(net, [LAN]);
    await routes.resolve();
    await routes.returnToFaster();
    expect(net.calls).toHaveLength(1);
  });

  it("tries every route again when all wait, so a single route can recover", async () => {
    const net = network({ [PAIRED]: "down" });
    const { routes } = routesWith(net);
    await expect(routes.resolve()).rejects.toBeInstanceOf(TypeError);
    net.answers[PAIRED] = "srv";
    expect(await routes.resolve()).toBe(PAIRED);
  });
});

describe("requests on a route", () => {
  it("rebuilds a signed request for another origin with its body", async () => {
    const signed = new Request(`${PAIRED}/api/x?a=1`, {
      method: "POST",
      body: "payload",
      headers: { "X-Studio-Signature": "sig" },
    });
    const moved = await retarget(signed, LAN);
    expect(moved.url).toBe(`${LAN}/api/x?a=1`);
    expect(moved.headers.get("X-Studio-Signature")).toBe("sig");
    expect(await moved.text()).toBe("payload");
  });

  it("puts a route that fails at the network level in cooldown", async () => {
    const net = network({ [PAIRED]: "srv", [LAN]: "srv" });
    const { routes } = routesWith(net, [LAN]);
    await routes.resolve();
    const send = vi.fn(async () => {
      throw new TypeError("network down");
    });
    await expect(
      sendRouted(routes, new Request(`${PAIRED}/api/x`), send),
    ).rejects.toBeInstanceOf(TypeError);
    expect(routes.current()).toBeUndefined();
    expect(await routes.resolve()).toBe(LAN);
  });

  it("does not blame a route for an abort", async () => {
    const net = network({ [PAIRED]: "srv" });
    const { routes } = routesWith(net);
    await routes.resolve();
    const send = vi.fn(async () => {
      throw new DOMException("aborted", "AbortError");
    });
    await expect(
      sendRouted(routes, new Request(`${PAIRED}/api/x`), send),
    ).rejects.toBeInstanceOf(DOMException);
    expect(routes.current()).toBe(PAIRED);
  });
});
