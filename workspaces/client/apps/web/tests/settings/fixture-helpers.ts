import type { WithAbsentFields } from "../accounts/fixture-helpers.ts";
import type { Server } from "node:http";
import type { ViteDevServer } from "vite";

export function fixtureUrl(server: Server | ViteDevServer, path = "") {
  const http = "httpServer" in server ? server.httpServer : server;
  // Every caller listens on an ephemeral TCP port before reading its address.
  const address = http!.address() as import("node:net").AddressInfo;
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

export async function fixtureFetchInput(
  input: RequestInfo | URL,
  options?: RequestInit,
  pathOnly = false,
) {
  if (!(input instanceof Request)) return { url: String(input), options };
  const url = new URL(input.url);
  return {
    url: pathOnly ? url.pathname + url.search : input.url,
    options: {
      method: input.method,
      ...(input.method === "GET" ? {} : { body: await input.clone().text() }),
    },
  };
}

// The renderer under test writes string bodies using the generated route contract.
// This one typed parser view covers both that input and its authored JSON document.
export const fixturePostBody = JSON.parse as <
  Path extends import("../../src/apiTypes.ts").ApiPostPath,
>(
  body: BodyInit | null | undefined,
) => WithAbsentFields<import("../../src/apiTypes.ts").PostBody<Path>>;

export function required<T>(value: T | null | undefined): T {
  if (value === null || value === undefined)
    throw new Error("Missing expected fixture value");
  return value;
}
