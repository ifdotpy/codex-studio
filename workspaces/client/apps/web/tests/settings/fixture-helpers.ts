import type { Server } from "node:http";
import type { ViteDevServer } from "vite";

export function fixtureUrl(server: Server | ViteDevServer, path = "") {
  const http = "httpServer" in server ? server.httpServer : server;
  const address = http?.address();
  if (!address || typeof address === "string")
    throw new Error("Fixture server has no TCP address");
  return `http://127.0.0.1:${address.port}/${path}`;
}

export function fixtureAccount(
  value: Pick<
    import("../../src/components/Accounts.tsx").Account,
    "id" | "label"
  > &
    Partial<import("../../src/components/Accounts.tsx").Account>,
): import("../../src/components/Accounts.tsx").Account {
  return {
    home: "",
    source: "Codex",
    provider: "codex",
    status: "ready",
    ...value,
  };
}

export function required<T>(value: T | null | undefined): T {
  if (value === null || value === undefined)
    throw new Error("Missing expected fixture value");
  return value;
}
