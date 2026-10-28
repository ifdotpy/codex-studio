import { afterEach, expect, it, vi } from "vitest";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
import type { CoreClient } from "../native/client";

const cores = vi.hoisted(() => new Map<string, unknown>());
vi.mock("../native/client", async (original) => ({
  ...(await original<typeof import("../native/client")>()),
  hasNativeClient: () => true,
  coreForServer: (server: string) => {
    if (!cores.has(server))
      cores.set(server, {
        server,
        request: vi.fn(async (method: string) => {
          if (method.startsWith("usage.")) return [];
          if (method !== "api.request") return null;
          return {
            status: 200,
            headers: {
              "content-type": "application/json",
              "x-studio-api-schema": API_SCHEMA_HASH,
            },
            base64: btoa(JSON.stringify({ workspaceId: server })),
          };
        }),
        listen: async () => () => {},
      });
    return cores.get(server);
  },
}));
import { createServerServices } from "./serverServices";
import { nativeDraftsHook } from "../native/useNativeDrafts";

afterEach(() => vi.unstubAllGlobals());

it("gives every server, local or paired, its own client core", async () => {
  vi.stubGlobal("window", {});
  const fetch = vi.fn();
  vi.stubGlobal("fetch", fetch);
  const remote = createServerServices({
    id: "server-a",
    label: "A",
    origin: "https://a.tailnet.ts.net",
    credentialId: "client-a",
  });
  const local = createServerServices({
    id: "local",
    label: "Local",
    origin: "http://localhost",
  });
  const core = cores.get("server-a") as CoreClient & {
    request: ReturnType<typeof vi.fn>;
  };
  expect(remote.core).toBe(core);
  expect(local.core).toBe(cores.get("local"));
  // The server's API reads go through its own core, never the network of the page.
  await expect(remote.api.get("/api/sync/identity")).resolves.toEqual({
    workspaceId: "server-a",
  });
  expect(core.request).toHaveBeenCalledWith(
    "api.request",
    expect.objectContaining({ path: "/api/sync/identity", method: "GET" }),
  );
  expect(fetch).not.toHaveBeenCalled();
  // Drafts belong to the core of the same server.
  expect(remote.useDrafts).toBe(nativeDraftsHook(core));
  expect(remote.useDrafts).not.toBe(local.useDrafts);
  remote.close();
  local.close();
});
