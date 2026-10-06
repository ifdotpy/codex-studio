// Review harness: real api.ts and sync/resourceEvents.ts with only transport,
// fetch, sync identity, and page globals stubbed. Readers use the ordinary API
// path; stream lifecycle changes must never leave a stored response behind.
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

class Source {
  static instances: Source[] = [];
  listeners = new Map<string, Set<(event: MessageEvent<string>) => void>>();
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) {
    Source.instances.push(this);
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
    const event = { data: JSON.stringify(value) } as MessageEvent<string>;
    for (const listener of this.listeners.get(name) || []) listener(event);
  }
  close() {
    this.closed = true;
  }
}

type Pending = { resolve: () => void; version: number };

async function setup() {
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
  const server = {
    version: 1,
    hold: false,
    fail: 0,
    calls: [] as string[],
    held: [] as Pending[],
  };
  vi.stubGlobal(
    "fetch",
    vi.fn(async (request: Request) => {
      const url = new URL(request.url);
      server.calls.push(url.pathname + url.search);
      // The server computes the body when the request arrives.
      const version = server.version;
      if (server.fail > 0) {
        server.fail--;
        return Response.json({ error: "boom" }, { status: 500 });
      }
      if (server.hold)
        await new Promise<void>((resolve, reject) => {
          server.held.push({ resolve, version });
          request.signal.addEventListener("abort", () =>
            reject(new DOMException("Aborted", "AbortError")),
          );
        });
      return Response.json({ version, accountKey: "a" });
    }),
  );
  const api = await import("./api");
  api.setToken("test-token");
  const resourceEvents = await import("./sync/resourceEvents");
  const { watchResourceChanges } = resourceEvents;
  const read = async (
    path: string,
    query?: Record<string, string>,
    force = false,
    signal?: AbortSignal,
  ): Promise<number> => {
    void force;
    const options = {
      ...(query ? { query } : {}),
      ...(signal ? { signal } : {}),
    };
    const result = (await (api.get as (...args: unknown[]) => Promise<unknown>)(
      path,
      options,
    )) as { version: number };
    return result.version;
  };
  let revision = 0;
  const emit = (
    reason: string,
    resources: unknown[],
    epoch = "epoch-1",
    source = Source.instances.at(-1)!,
  ) => {
    const eventRevision = ++revision;
    source.emit("resources", {
      protocol: 3,
      workspaceId,
      epoch,
      revision: eventRevision,
      reason,
      resources,
      resourceVersions: resources.map((resource) => ({
        resource,
        revision: eventRevision,
      })),
    });
  };
  return {
    server,
    read,
    emit,
    watchResourceChanges,
    acknowledgeEntitySequence: resourceEvents.acknowledgeEntitySequence,
  };
}

const tick = (ms = 300) => vi.advanceTimersByTimeAsync(ms);

describe("resource freshness across stream lifecycle changes", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    Source.instances = [];
    syncDatabase.mockClear();
    resumeListeners.clear();
  });

  it("A: re-reads fresh data after the event stream dropped and reconnected", async () => {
    const { server, read, emit, watchResourceChanges } = await setup();
    const seen: number[] = [];
    const costs = { kind: "costs" };
    watchResourceChanges(costs as never, async () => {
      seen.push(await read("/api/costs", { account_key: "a" }));
    });
    await tick(250);
    expect(Source.instances).toHaveLength(1);
    emit("initial", [costs]);
    await tick();
    expect(seen).toEqual([1]);

    // Stream drops; the server changes costs while nobody is connected.
    Source.instances[0]!.onerror?.();
    server.version = 2;
    await tick(2000);
    expect(Source.instances).toHaveLength(2);
    // A new EventSource carries no Last-Event-ID, so hub.py answers "initial".
    emit("initial", [costs]);
    await tick();
    console.log("A", JSON.stringify({ seen, calls: server.calls }));
    expect(seen.at(-1)).toBe(2);
  });

  it("B: re-reads fresh data after a server restart (new epoch)", async () => {
    const { server, read, emit, watchResourceChanges } = await setup();
    const seen: number[] = [];
    const desktop = { kind: "desktop" };
    watchResourceChanges(desktop as never, async () => {
      seen.push(await read("/api/desktop"));
    });
    await tick(250);
    emit("initial", [desktop]);
    await tick();
    Source.instances[0]!.onerror?.();
    server.version = 2;
    await tick(2000);
    emit("initial", [desktop], "epoch-2");
    await tick();
    console.log("B", JSON.stringify({ seen, calls: server.calls }));
    expect(seen.at(-1)).toBe(2);
  });

  it("C: re-reads fresh data when a resource is watched again after a gap", async () => {
    const { server, read, emit, watchResourceChanges } = await setup();
    const seen: number[] = [];
    const limits = { kind: "limits", accountKey: "b" };
    const anchor = watchResourceChanges(
      { kind: "accounts" } as never,
      async () => {},
    );
    const readLimits = async () => {
      seen.push(await read("/api/limits", { account_key: "b" }));
    };
    const stop = watchResourceChanges(limits as never, readLimits);
    await tick(250);
    emit("initial", [{ kind: "accounts" }, limits]);
    await tick();
    expect(seen).toEqual([1]);
    // Chat switches to another account: limits for "b" leave the subscription.
    stop();
    await tick();
    server.version = 2; // hub.py sends this change only to subscribers of b
    // Chat switches back.
    watchResourceChanges(limits as never, readLimits);
    await tick();
    emit("initial", [{ kind: "accounts" }, limits]);
    await tick();
    console.log("C", JSON.stringify({ seen, calls: server.calls }));
    expect(seen.at(-1)).toBe(2);
    anchor();
  });

  it("D: a watcher that fires on a change event does not show the pre-change response of another caller", async () => {
    const { server, read, emit, watchResourceChanges } = await setup();
    const seen: number[] = [];
    const accounts = { kind: "accounts" };
    watchResourceChanges(accounts as never, async () => {
      seen.push(await read("/api/accounts"));
    });
    await tick(250);
    emit("initial", [accounts]);
    await tick();
    expect(seen).toEqual([1]);
    emit("change", [accounts]);
    await tick();
    expect(seen).toEqual([1, 1]);

    // Another caller (dialog open, forced) starts a slow read.
    server.hold = true;
    const other = read("/api/accounts", undefined, true);
    await tick(0);
    expect(server.held).toHaveLength(1);
    // A mutation commits after the server built that response.
    server.version = 2;
    emit("change", [accounts]);
    await tick();
    for (const held of server.held.splice(0)) held.resolve();
    await tick();
    for (const held of server.held.splice(0)) held.resolve();
    await tick();
    console.log(
      "D",
      JSON.stringify({ seen, other: await other, calls: server.calls }),
    );
    expect(seen.at(-1)).toBe(2);
  });

  it("E: an unforced refresh after a mutation shows the mutation when no event arrived yet", async () => {
    // Mirrors AccountSignIn.tsx: await post(login); await state.refresh().
    const { server, read } = await setup();
    expect(await read("/api/accounts")).toBe(1);
    server.version = 2; // POST committed, response received, event not yet delivered
    const after = await read("/api/accounts");
    console.log("E", JSON.stringify({ after, calls: server.calls }));
    expect(after).toBe(2);
  });

  it("F: data is not served from memory a day later without any event", async () => {
    const { server, read } = await setup();
    expect(await read("/api/desktop", { account_key: "a" })).toBe(1);
    server.version = 2;
    await tick(24 * 3600 * 1000);
    const after = await read("/api/desktop", { account_key: "a" });
    console.log("F", JSON.stringify({ after, calls: server.calls }));
    expect(after).toBe(2);
  });

  it("G: one caller aborting does not fail an independent read", async () => {
    const { server, read } = await setup();
    server.hold = true;
    const controller = new AbortController();
    const first = read(
      "/api/models",
      { account_key: "a" },
      false,
      controller.signal,
    );
    const second = read("/api/models", { account_key: "a" });
    const outcomes = Promise.allSettled([first, second]);
    await tick(250);
    controller.abort();
    await tick();
    for (const held of server.held.splice(0)) held.resolve();
    await tick();
    const [a, b] = await outcomes;
    console.log(
      "G",
      JSON.stringify({
        first: a.status,
        second: b.status,
        calls: server.calls,
      }),
    );
    expect(b.status).toBe("fulfilled");
  });

  it("H: a forced read started after a change does not return an earlier response", async () => {
    const { server, read } = await setup();
    server.hold = true;
    const early = read("/api/limits", { account_key: "a" });
    await tick(250);
    server.version = 2; // e.g. reset credit redeemed
    const forced = read("/api/limits", { account_key: "a" }, true);
    await tick(0);
    for (const held of server.held.splice(0)) held.resolve();
    await tick();
    for (const held of server.held.splice(0)) held.resolve();
    console.log(
      "H",
      JSON.stringify({
        early: await early,
        forced: await forced,
        calls: server.calls,
      }),
    );
    expect(await forced).toBe(2);
  });

  it("I: a failed read is retried on the next request", async () => {
    const { server, read } = await setup();
    server.fail = 1;
    const failed = await Promise.allSettled([
      read("/api/costs", { account_key: "a" }),
    ]);
    const retry = await read("/api/costs", { account_key: "a" });
    console.log(
      "I",
      JSON.stringify({
        results: failed.map((item) => item.status),
        retry,
        calls: server.calls,
      }),
    );
    expect(failed.map((item) => item.status)).toEqual(["rejected"]);
    expect(retry).toBe(1);
    expect(server.calls).toHaveLength(2);
  });

  it("J: a response that started before a change cannot hide its fresh read", async () => {
    const { server, read, emit, watchResourceChanges } = await setup();
    const costs = { kind: "costs" };
    watchResourceChanges(costs as never, async () => {});
    await tick(250);
    emit("initial", [costs]);
    await tick();
    server.hold = true;
    const early = read("/api/costs", { account_key: "a" });
    await tick(0);
    server.version = 2;
    emit("change", [costs]);
    await tick();
    for (const held of server.held.splice(0)) held.resolve();
    await tick();
    server.hold = false;
    const later = await read("/api/costs", { account_key: "a" });
    console.log(
      "J",
      JSON.stringify({ early: await early, later, calls: server.calls }),
    );
    expect(later).toBe(2);
  });

  it("K: an unsubscribed resource is refreshed on its next request", async () => {
    const { server, read, emit, watchResourceChanges } = await setup();
    watchResourceChanges({ kind: "accounts" } as never, async () => {});
    await tick(250);
    emit("initial", [{ kind: "accounts" }]);
    await tick();
    expect(await read("/api/desktop", { account_key: "a" })).toBe(1);
    const url = new URL(Source.instances.at(-1)!.url, "http://studio.test");
    server.version = 2;
    // An account change (reconnect, login) does reach this tab.
    emit("change", [{ kind: "accounts" }]);
    await tick();
    const after = await read("/api/desktop", { account_key: "a" });
    console.log(
      "K",
      JSON.stringify({
        subscribed: url.searchParams.get("resources"),
        after,
        calls: server.calls,
      }),
    );
    expect(after).toBe(2);
  });

  it("does not pull entity rows already delivered in the acting tab response", async () => {
    const { emit, watchResourceChanges, acknowledgeEntitySequence } =
      await setup();
    const state = { kind: "state" };
    let invalidations = 0;
    watchResourceChanges(state as never, () => invalidations++);
    await tick(250);
    emit("initial", [state]);
    await tick();
    expect(invalidations).toBe(1);
    window.dispatchEvent(new Event("codex-api-mutation-start"));
    emit("change", [state]);
    await tick();
    expect(invalidations).toBe(1);
    acknowledgeEntitySequence(2, workspaceId);
    window.dispatchEvent(new Event("codex-api-mutation-end"));
    await tick();
    expect(invalidations).toBe(1);
  });
});
