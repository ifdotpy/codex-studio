import assert from "node:assert/strict";
/// <reference lib="es2024.promise" />
import { afterEach, expect, test, vi } from "vitest";
import {
  refreshServerName,
  validateServerName,
  watchServerName,
} from "./serverNames";
import { readServers, SERVER_REGISTRY_KEY } from "./registry";
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
  assert.ok(fetch.mock.calls[0] != null);
  expect(fetch.mock.calls[0][0]).toBe(server);
  expect(readServers()).toEqual([{ ...server, label: "Igor MBP" }]);
  fetch.mockImplementation(
    async () =>
      new Response(JSON.stringify({ serverId: "different", label: "Wrong" })),
  );
  await expect(refreshServerName(server)).rejects.toThrow("another identity");
  expect(readServers()[0]?.label).toBe("Igor MBP");
});

test("saved server names use their own stream with zero idle status reads", async () => {
  vi.useFakeTimers();
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
  vi.stubGlobal("window", new EventTarget());
  const fetch = vi.fn();
  setServerCredentialAdapter({
    fetch,
    forget: async () => {},
    hasPairAttempt: async () => false,
    pair: async () => server,
  });
  let accept!: (
    version?: import("../sync/resourceEvents").ResourceVersion,
  ) => void;
  const release = vi.fn();
  const watchResourceChanges = vi.fn((_resource, callback) => {
    accept = callback;
    return release;
  });
  const stop = watchServerName(server, { watchResourceChanges });
  assert.ok(watchResourceChanges.mock.calls[0] != null);
  expect(watchResourceChanges.mock.calls[0][0]).toEqual({
    kind: "multi-server",
  });
  await vi.advanceTimersByTimeAsync(5 * 60_000);
  expect(fetch).not.toHaveBeenCalled();
  const deliver = (id: string, label: string) =>
    accept({
      epoch: "owner",
      revision: 1,
      discoverySnapshot: { identity: { serverId: id, label } },
    } as import("../sync/resourceEvents").ResourceVersion);
  deliver("other", "Wrong");
  expect(readServers()[0]?.label).toBe("Old");
  deliver("remote", "New");
  expect(readServers()[0]?.label).toBe("New");
  // An old stream cannot restore a forgotten peer or overwrite a replacement.
  values.set(
    SERVER_REGISTRY_KEY,
    JSON.stringify([
      { ...server, label: "Replacement", credentialId: "replacement" },
    ]),
  );
  deliver("remote", "Stale");
  expect(readServers()[0]?.label).toBe("Replacement");
  values.set(SERVER_REGISTRY_KEY, "[]");
  deliver("remote", "Removed");
  expect(readServers()).toEqual([]);
  stop();
  expect(release).toHaveBeenCalledTimes(1);
  values.set(SERVER_REGISTRY_KEY, JSON.stringify([server]));
  deliver("remote", "Stopped");
  expect(readServers()[0]?.label).toBe("Old");
  expect(fetch).not.toHaveBeenCalled();
});
