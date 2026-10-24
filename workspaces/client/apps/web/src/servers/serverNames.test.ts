/// <reference lib="es2024.promise" />
import { afterEach, expect, test, vi } from "vitest";
import {
  refreshServerName,
  validateServerName,
  watchRegisteredServerNames,
} from "./serverNames";
import {
  readServers,
  SERVER_REGISTRY_EVENT,
  SERVER_REGISTRY_KEY,
} from "./registry";
import { setServerCredentialAdapter } from "./transport";

test("server names accept visible Unicode and reject controls and outer spaces", () => {
  for (const name of [
    "Lumina Mac",
    "Igor MBP",
    "Kukuka WSL",
    "Kukuka Windows",
    "Écran",
    "💻".repeat(80),
  ])
    expect(() => validateServerName(name)).not.toThrow();
  for (const name of [
    "",
    " ",
    " outer",
    "outer ",
    "a".repeat(81),
    "💻".repeat(81),
    "line\nname",
    "hidden\u202ename",
    "tab\tname",
    "nul\0name",
    "space\u00a0name",
  ])
    expect(() => validateServerName(name)).toThrow();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

test("a signed status refresh changes only the name of its registered server", async () => {
  const server = {
    id: "remote",
    label: "Old",
    origin: "https://remote.tailnet.ts.net",
    credentialId: "credential",
    alias: "MBP",
  };
  const values = new Map([[SERVER_REGISTRY_KEY, JSON.stringify([server])]]);
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
  });
  const fetch = vi.fn(
    async (_server: unknown, _request: Request) =>
      new Response(JSON.stringify({ serverId: "remote", label: "Igor MBP" })),
  );
  setServerCredentialAdapter({
    fetch,
    forget: async () => {},
    hasPairAttempt: async () => false,
    pair: async () => server,
  });
  await refreshServerName(server);
  expect(fetch.mock.calls[0][0]).toBe(server);
  expect(readServers()).toEqual([{ ...server, label: "Igor MBP" }]);
  fetch.mockImplementation(
    async () =>
      new Response(JSON.stringify({ serverId: "different", label: "Wrong" })),
  );
  await expect(refreshServerName(server)).rejects.toThrow("another identity");
  expect(readServers()[0].label).toBe("Igor MBP");
});

test("an empty registered server roster has no recurring status reads", async () => {
  vi.useFakeTimers();
  const values = new Map<string, string>();
  const getItem = vi.fn((key: string) => values.get(key) ?? null);
  vi.stubGlobal("localStorage", {
    getItem,
    setItem: (key: string, value: string) => values.set(key, value),
  });
  vi.stubGlobal("window", new EventTarget());
  const refresh = vi.fn(async () => {});

  const stop = watchRegisteredServerNames(refresh);
  expect(getItem).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(5 * 60_000);

  expect(refresh).not.toHaveBeenCalled();
  expect(getItem).toHaveBeenCalledTimes(2);
  expect(vi.getTimerCount()).toBe(0);
  stop();
  window.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
  window.dispatchEvent(
    Object.assign(new Event("storage"), { key: SERVER_REGISTRY_KEY }),
  );
  expect(getItem).toHaveBeenCalledTimes(2);
});

test("registry and filtered storage events wake reads and retain peer refresh", async () => {
  vi.useFakeTimers();
  const values = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
  });
  const events = new EventTarget();
  vi.stubGlobal("window", events);
  const refresh = vi.fn(async () => {});
  const server = {
    id: "remote",
    label: "Remote",
    origin: "https://remote.tailnet.ts.net",
    credentialId: "credential",
  };
  const stop = watchRegisteredServerNames(refresh);

  values.set(SERVER_REGISTRY_KEY, JSON.stringify([server]));
  const unrelatedStorage = Object.assign(new Event("storage"), {
    key: "unrelated-key",
  });
  events.dispatchEvent(unrelatedStorage);
  expect(refresh).not.toHaveBeenCalled();

  const registryStorage = Object.assign(new Event("storage"), {
    key: SERVER_REGISTRY_KEY,
  });
  events.dispatchEvent(registryStorage);
  expect(refresh).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(30_000);
  expect(refresh).toHaveBeenCalledTimes(2);

  values.set(SERVER_REGISTRY_KEY, "[]");
  events.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
  await vi.advanceTimersByTimeAsync(5 * 60_000);
  expect(refresh).toHaveBeenCalledTimes(2);
  expect(vi.getTimerCount()).toBe(0);
  stop();
});

test("an in-flight name response does not restore a removed peer", async () => {
  const server = {
    id: "remote",
    label: "Old",
    origin: "https://remote.tailnet.ts.net",
    credentialId: "credential",
  };
  const values = new Map([[SERVER_REGISTRY_KEY, JSON.stringify([server])]]);
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
  });
  const events = new EventTarget();
  vi.stubGlobal("window", events);
  const entered = Promise.withResolvers<void>();
  let finish!: (response: Response) => void;
  setServerCredentialAdapter({
    fetch: vi.fn(
      () =>
        new Promise<Response>((resolve) => {
          finish = resolve;
          entered.resolve();
        }),
    ),
    forget: async () => {},
    hasPairAttempt: async () => false,
    pair: async () => server,
  });

  let finishRefresh!: () => void;
  const refreshFinished = new Promise<void>(
    (resolve) => (finishRefresh = resolve),
  );
  const stop = watchRegisteredServerNames(async (peer) => {
    try {
      await refreshServerName(peer);
    } finally {
      finishRefresh();
    }
  });
  await entered.promise;
  values.set(SERVER_REGISTRY_KEY, "[]");
  events.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
  finish(new Response(JSON.stringify({ serverId: "remote", label: "New" })));
  await refreshFinished;

  expect(readServers()).toEqual([]);
  stop();
});

test("an old name response cannot overwrite a same-ID replacement peer", async () => {
  const original = {
    id: "remote",
    label: "Original peer",
    origin: "https://old.remote.tailnet.ts.net",
    credentialId: "old-credential",
  };
  const replacement = {
    ...original,
    label: "Replacement peer",
    origin: "https://new.remote.tailnet.ts.net",
    credentialId: "new-credential",
  };
  const values = new Map([[SERVER_REGISTRY_KEY, JSON.stringify([original])]]);
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
  });
  const events = new EventTarget();
  vi.stubGlobal("window", events);
  const entered = Promise.withResolvers<void>();
  let finishOldRequest!: (response: Response) => void;
  const requestedPeers: string[] = [];
  setServerCredentialAdapter({
    fetch: vi.fn((server) => {
      requestedPeers.push(server.origin);
      if (requestedPeers.length === 1)
        return new Promise<Response>((resolve) => {
          finishOldRequest = resolve;
          entered.resolve();
        });
      return Promise.reject(new Error("replacement is offline"));
    }),
    forget: async () => {},
    hasPairAttempt: async () => false,
    pair: async () => replacement,
  });

  let finishRefreshes!: () => void;
  const refreshesFinished = new Promise<void>((resolve) => {
    finishRefreshes = resolve;
  });
  let completedRefreshes = 0;
  const stop = watchRegisteredServerNames(async (peer) => {
    try {
      await refreshServerName(peer);
    } finally {
      completedRefreshes += 1;
      if (completedRefreshes === 2) finishRefreshes();
    }
  });
  await entered.promise;
  values.set(SERVER_REGISTRY_KEY, JSON.stringify([replacement]));
  events.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
  finishOldRequest(
    new Response(JSON.stringify({ serverId: "remote", label: "Stale name" })),
  );
  await refreshesFinished;
  expect(requestedPeers).toEqual([original.origin, replacement.origin]);

  expect(readServers()[0]).toMatchObject({
    label: replacement.label,
    origin: replacement.origin,
    credentialId: replacement.credentialId,
  });
  stop();
});

test("saved servers stay idle without periodic status reads", async () => {
  vi.useFakeTimers();
  const server = {
    id: "remote",
    label: "Remote",
    origin: "https://remote.tailnet.ts.net",
    credentialId: "credential",
  };
  vi.stubGlobal("localStorage", {
    getItem: (key: string) =>
      key === SERVER_REGISTRY_KEY ? JSON.stringify([server]) : null,
    setItem: vi.fn(),
  });
  vi.stubGlobal("window", new EventTarget());
  const refresh = vi.fn(async () => {});
  const stop = watchRegisteredServerNames(refresh);
  await vi.advanceTimersByTimeAsync(0);
  refresh.mockClear();
  await vi.advanceTimersByTimeAsync(5 * 60_000);
  expect(refresh).not.toHaveBeenCalled();
  stop();
});
