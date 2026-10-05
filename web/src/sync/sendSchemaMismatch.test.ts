import { afterEach, expect, it, vi } from "vitest";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";

const { syncDatabase } = vi.hoisted(() => ({ syncDatabase: vi.fn() }));
vi.mock("./client", async (load) => ({
  ...(await load<typeof import("./client")>()),
  syncDatabase,
}));

afterEach(() => {
  vi.resetModules();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  syncDatabase.mockReset();
});

it("keeps a locally schema-blocked deliver queued with the same id, then sends once after reload", async () => {
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("location", { origin: "http://studio.test" });
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  });
  vi.stubGlobal("document", {
    documentElement: { dataset: {} },
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("localStorage", { getItem: () => null, setItem: vi.fn() });
  syncDatabase.mockResolvedValue({ workspaceId: "workspace-a" });
  const requests: string[] = [];
  let mismatch = true;
  vi.stubGlobal("fetch", async (request: Request) => {
    const url = new URL(request.url);
    requests.push(`${request.method} ${url.pathname}`);
    if (url.pathname === "/api/sync/identity")
      return Response.json(
        { workspaceId: "workspace-a" },
        {
          headers: {
            [API_SCHEMA_HASH_HEADER]: mismatch ? "foreign" : API_SCHEMA_HASH,
          },
        },
      );
    if (url.pathname === "/api/session")
      return Response.json(
        { token: "token" },
        {
          headers: {
            [API_SCHEMA_HASH_HEADER]: mismatch ? "foreign" : API_SCHEMA_HASH,
          },
        },
      );
    if (url.pathname === "/api/messages")
      return Response.json(
        { id: "stable-id", status: "delivered" },
        {
          headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
        },
      );
    throw new Error(`Unexpected request ${url.pathname}`);
  });

  const record = {
    id: "stable-id",
    payload: JSON.stringify({
      body: { id: "stable-id", room: "chat", text: "hello", delivery: "queue" },
      status: "queued",
      attempted: false,
      created: 1,
    }),
  };
  const doc = {
    id: "stable-id",
    getLatest: () => record,
    incrementalModify: async (
      change: (value: typeof record) => typeof record,
    ) => {
      Object.assign(record, change({ ...record }));
      return record;
    },
  };
  let send = await import("./send");
  expect(await send.deliver(doc)).toMatchObject({
    id: "stable-id",
    queued: true,
  });
  expect(JSON.parse(record.payload)).toMatchObject({
    status: "queued",
    attempted: true,
    body: { id: "stable-id", text: "hello" },
  });
  expect(
    requests.filter((item) => item.endsWith("/api/messages")),
  ).toHaveLength(0);
  expect(requests).toHaveLength(2);

  mismatch = false;
  vi.resetModules();
  send = await import("./send");
  expect(await send.deliver(doc)).toMatchObject({
    id: "stable-id",
    status: "delivered",
  });
  expect(
    requests.filter((item) => item.endsWith("/api/messages")),
  ).toHaveLength(1);
  expect(JSON.parse(record.payload)).toMatchObject({
    status: "accepted",
    body: { id: "stable-id", text: "hello" },
  });
});
