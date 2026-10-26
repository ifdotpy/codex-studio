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
  return Object.assign(
    {
      home: "",
      source: "",
      provider: "codex" as const,
      status: "ready" as const,
    },
    value,
  );
}

export function required<T>(value: T | null | undefined): T {
  if (value === null || value === undefined)
    throw new Error("Missing expected fixture value");
  return value;
}

export function fixtureNotReady(): never {
  throw new Error("Fixture harness has not rendered");
}

export async function fixtureFetchInput(
  input: RequestInfo | URL,
  options?: RequestInit,
) {
  if (!(input instanceof Request)) return { url: String(input), options };
  const url = new URL(input.url);
  return {
    url: url.pathname + url.search,
    options: {
      method: input.method,
      ...(input.method === "GET" ? {} : { body: await input.clone().text() }),
    },
  };
}

export function fixturePostBody<
  Path extends import("../../src/apiTypes.ts").ApiPostPath,
>(
  body: BodyInit | null | undefined,
): import("../../src/apiTypes.ts").PostBody<Path> {
  // The renderer under test wrote this request using the generated route contract.
  return JSON.parse(
    String(body),
  ) as import("../../src/apiTypes.ts").PostBody<Path>;
}
