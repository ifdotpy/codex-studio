import { afterEach, expect, test, vi } from "vitest";
import { refreshServerName, validateServerName } from "./serverNames";
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

afterEach(() => vi.unstubAllGlobals());

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
