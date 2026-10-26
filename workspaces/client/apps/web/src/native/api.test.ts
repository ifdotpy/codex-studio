import { expect, it, vi } from "vitest";
import { createNativeApi } from "./api";
import { ApiError, ApiSchemaMismatchError } from "../apiErrors";
import { API_SCHEMA_HASH } from "../generated/apiSchema";
import { NativeCommandError } from "./commandError";
import { NetworkTimeoutError } from "../apiErrors";

it("preserves the shared timeout error without a renderer retry", async () => {
  const request = vi
    .fn()
    .mockRejectedValue(
      new NativeCommandError({
        code: "timeout",
        message: "unknown native response",
      }),
    );
  const api = createNativeApi({ request } as any, storage());
  await expect(api.get("/api/accounts")).rejects.toBeInstanceOf(
    NetworkTimeoutError,
  );
  expect(request).toHaveBeenCalledTimes(1);
});

function storage() {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
  };
}
it("sends typed queries to Rust and preserves conditional read metadata", async () => {
  const request = vi
    .fn()
    .mockResolvedValueOnce({
      status: 200,
      headers: { "x-studio-api-schema": API_SCHEMA_HASH, etag: "one" },
      body: { accounts: [] },
    })
    .mockResolvedValueOnce({
      status: 304,
      headers: { "x-studio-api-schema": API_SCHEMA_HASH, etag: "one" },
      body: null,
    });
  const api = createNativeApi({ request } as any, storage());
  const metadata = {};
  expect(
    await api.get("/api/accounts", {
      query: { key: "a & b" },
      readMetadata: metadata,
    } as any),
  ).toEqual({ accounts: [] });
  expect(metadata).toEqual({ etag: "one", notModified: false });
  expect(request.mock.calls[0][1]).toMatchObject({
    path: "/api/accounts",
    query: { key: "a & b" },
    apiSchema: API_SCHEMA_HASH,
    jsonResponse: true,
  });
  expect(
    await api.get("/api/accounts", {
      etag: "one",
      readMetadata: metadata,
    } as any),
  ).toBeUndefined();
  expect(metadata).toEqual({ etag: "one", notModified: true });
});

it("commits exact monitor identity before HTTP and retains uncertainty without retry", async () => {
  let release!: (value: unknown) => void;
  const committed = new Promise((resolve) => {
    release = resolve;
  });
  const request = vi.fn(async (method: string) => {
    if (method === "monitor_input.begin") return committed;
    if (method === "api.request") throw new Error("response lost");
    return true;
  });
  const api = createNativeApi({ request } as any, storage());
  const sent = api.post("/api/monitor/input", {
    id: "monitor",
    text: "synthetic",
  });
  expect(request).toHaveBeenCalledTimes(1);
  release({
    requestId: "exact",
    payloadDigest: "a".repeat(64),
    uncertain: false,
  });
  await expect(sent).rejects.toThrow("response lost");
  expect(request.mock.calls.map(([method]) => method)).toEqual([
    "monitor_input.begin",
    "api.request",
    "monitor_input.unknown",
  ]);
  expect(request.mock.calls[1]).toEqual([
    "api.request",
    expect.objectContaining({
      requestId: "exact",
      body: { id: "monitor", text: "synthetic" },
    }),
    "exact",
  ]);
});

it("keeps API error identity and refuses further requests after schema mismatch", async () => {
  const request = vi
    .fn()
    .mockResolvedValueOnce({
      status: 409,
      headers: {},
      body: { error: "conflict", outcome: "not_applied" },
    })
    .mockResolvedValueOnce({
      status: 426,
      headers: { "x-studio-api-schema-mismatch": "1" },
      body: { error: "update" },
    });
  const api = createNativeApi({ request } as any, storage());
  await expect(api.get("/api/accounts")).rejects.toBeInstanceOf(ApiError);
  await expect(api.get("/api/accounts")).rejects.toBeInstanceOf(
    ApiSchemaMismatchError,
  );
  await expect(api.get("/api/accounts")).rejects.toBeInstanceOf(
    ApiSchemaMismatchError,
  );
  expect(request).toHaveBeenCalledTimes(2);
});
