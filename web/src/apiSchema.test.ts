import { afterEach, expect, it, vi } from "vitest";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
  API_SCHEMA_MISMATCH_HEADER,
} from "./generated/apiSchema";

afterEach(() => {
  vi.resetModules();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function stubBrowser() {
  vi.stubGlobal("location", { origin: "http://studio.test" });
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  });
  vi.stubGlobal("document", {
    hidden: false,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("navigator", { onLine: true });
}

it("accepts a matching server schema hash without entering mismatch state", async () => {
  stubBrowser();
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response(JSON.stringify({ token: "session-token" }), {
        headers: {
          "Content-Type": "application/json",
          [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH,
        },
      }),
  );
  const api = await import("./api");
  await expect(api.get("/api/session")).resolves.toEqual({
    token: "session-token",
  });
  expect(api.isApiSchemaMismatch()).toBe(false);
});

it("sends the generated schema hash and raises mismatch state from an API response", async () => {
  const requests: Request[] = [];
  stubBrowser();
  vi.stubGlobal("fetch", async (request: Request) => {
    requests.push(request);
    return new Response(JSON.stringify({ token: "session-token" }), {
      status: 200,
      headers: {
        "Content-Type": "application/json",
        [API_SCHEMA_HASH_HEADER]: "foreign-schema",
      },
    });
  });

  const api = await import("./api");
  const mismatch = vi.fn();
  api.onApiSchemaMismatch(mismatch);
  await expect(api.get("/api/session")).resolves.toEqual({
    token: "session-token",
  });

  expect(requests).toHaveLength(1);
  expect(requests[0]!.headers.get(API_SCHEMA_HASH_HEADER)).toBe(
    API_SCHEMA_HASH,
  );
  expect(api.isApiSchemaMismatch()).toBe(true);
  expect(mismatch).toHaveBeenCalledTimes(1);
  await expect(
    api.syncGet("/api/limits", { query: { account_key: "default" } }),
  ).rejects.toMatchObject({ status: 426 });
  await expect(api.get("/api/session")).rejects.toMatchObject({
    name: "ApiSchemaMismatchError",
  });
  expect(requests).toHaveLength(1);
});

it("blocks later writes when a rollback returns an identity without a schema hash", async () => {
  stubBrowser();
  const fetch = vi.fn(
    async () =>
      new Response(
        JSON.stringify({
          workspaceId: "workspace",
          syncProtocol: 2,
          chatState: true,
        }),
        { headers: { "Content-Type": "application/json" } },
      ),
  );
  vi.stubGlobal("fetch", fetch);
  const api = await import("./api");
  const mismatch = vi.fn();
  api.onApiSchemaMismatch(mismatch);

  await api.syncGet("/api/sync/identity");

  expect(api.isApiSchemaMismatch()).toBe(true);
  expect(mismatch).toHaveBeenCalledTimes(1);
  await expect(
    api.post("/api/messages", {
      id: "same-message",
      room: "chat",
      text: "hello",
    }),
  ).rejects.toMatchObject({ name: "ApiSchemaMismatchError" });
  expect(fetch).toHaveBeenCalledTimes(1);
});

it("accepts the identity when its server schema hash matches", async () => {
  stubBrowser();
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response(
        JSON.stringify({ workspaceId: "workspace", syncProtocol: 2 }),
        {
          headers: {
            "Content-Type": "application/json",
            [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH,
          },
        },
      ),
  );
  const api = await import("./api");

  await api.syncGet("/api/sync/identity");

  expect(api.isApiSchemaMismatch()).toBe(false);
});

it("does not treat an unmarked HTTP 426 as an API schema mismatch", async () => {
  stubBrowser();
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response(JSON.stringify({ detail: "Unsupported stream protocol" }), {
        status: 426,
        headers: { "Content-Type": "application/json" },
      }),
  );
  const api = await import("./api");
  const request = api.post("/api/messages", {
    id: "m1",
    room: "r",
    text: "x",
  });
  await expect(request).rejects.toMatchObject({ status: 426 });
  const error = await request.catch((reason: unknown) => reason);
  expect(error).toBeInstanceOf(api.ApiError);
  expect(error).not.toBeInstanceOf(api.ApiSchemaMismatchError);
  expect(api.isApiSchemaMismatch()).toBe(false);
});

it("raises mismatch state only for the explicitly marked schema-gated 426", async () => {
  stubBrowser();
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response(JSON.stringify({ error: "Unsupported protocol" }), {
        status: 426,
      }),
  );
  let api = await import("./api");
  await expect(api.get("/api/session")).rejects.toMatchObject({ status: 426 });
  expect(api.isApiSchemaMismatch()).toBe(false);

  vi.resetModules();
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response(JSON.stringify({ error: "Reload Studio" }), {
        status: 426,
        headers: { [API_SCHEMA_MISMATCH_HEADER]: "1" },
      }),
  );
  api = await import("./api");
  await expect(
    api.post("/api/messages", { id: "message-1", room: "chat", text: "hello" }),
  ).rejects.toMatchObject({ status: 426 });
  expect(api.isApiSchemaMismatch()).toBe(true);
});

it("clears the update attempt only after a matching response on the reloaded page", async () => {
  const storage = new Map<string, string>();
  vi.stubGlobal("sessionStorage", {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key),
  });
  const responses = ["foreign-schema", API_SCHEMA_HASH];
  stubBrowser();
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response(JSON.stringify({ token: "session-token" }), {
        status: 200,
        headers: {
          "Content-Type": "application/json",
          [API_SCHEMA_HASH_HEADER]: responses.shift()!,
        },
      }),
  );
  const api = await import("./api");
  await api.get("/api/session");
  storage.set("studio-api-schema-update-attempted", "1");
  expect(api.schemaUpdateFailedAfterReload()).toBe(true);
  vi.resetModules();
  responses[0] = API_SCHEMA_HASH;
  const reloadedApi = await import("./api");
  await reloadedApi.get("/api/session");
  expect(storage.has("studio-api-schema-update-attempted")).toBe(false);
});

it("reloads only after Update and distinguishes a matching renderer from a persistent mismatch", async () => {
  const storage = new Map<string, string>();
  const reload = vi.fn();
  vi.stubGlobal("sessionStorage", {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key),
  });
  stubBrowser();
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
    location: { reload },
  });
  let responseHash = "foreign-schema";
  vi.stubGlobal(
    "fetch",
    async () =>
      new Response("{}", {
        headers: { [API_SCHEMA_HASH_HEADER]: responseHash },
      }),
  );

  let api = await import("./api");
  await api.get("/api/session");
  expect(reload).not.toHaveBeenCalled();
  await api.updateRendererAndReload();
  expect(reload).toHaveBeenCalledTimes(1);
  expect(storage.has("studio-api-schema-update-attempted")).toBe(true);

  vi.resetModules();
  responseHash = API_SCHEMA_HASH;
  api = await import("./api");
  await api.get("/api/session");
  expect(api.isApiSchemaMismatch()).toBe(false);
  expect(api.schemaUpdateFailedAfterReload()).toBe(false);

  storage.set("studio-api-schema-update-attempted", "1");
  vi.resetModules();
  responseHash = "foreign-schema";
  api = await import("./api");
  await api.get("/api/session");
  expect(api.isApiSchemaMismatch()).toBe(true);
  expect(api.schemaUpdateFailedAfterReload()).toBe(true);
  expect(reload).toHaveBeenCalledTimes(1);
});

it("clears the completed-update marker when service-worker update fails", async () => {
  const storage = new Map<string, string>();
  stubBrowser();
  vi.stubGlobal("sessionStorage", {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key),
  });
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
    location: { href: "http://studio.test/", assign: vi.fn() },
  });
  vi.stubGlobal("navigator", {
    onLine: true,
    serviceWorker: {
      getRegistration: vi.fn().mockResolvedValue({
        update: vi.fn().mockRejectedValue(new Error("offline")),
      }),
    },
  });
  const api = await import("./api");
  await expect(api.updateRendererAndReload()).rejects.toThrow("offline");
  expect(storage.has("studio-api-schema-update-attempted")).toBe(false);
});
