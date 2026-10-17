import { afterEach, describe, expect, it, vi } from "vitest";
import { nativeFetch } from "./http";

afterEach(() => vi.unstubAllGlobals());
const origin = "http://localhost";
function host(
  reply: (request: { id: string; params: Record<string, unknown> }) => unknown,
) {
  const invoke = vi.fn(async (_command, args) => reply(args.request));
  vi.stubGlobal("__TAURI__", { core: { invoke } });
  return invoke;
}

describe("native HTTP transport", () => {
  it("preserves HTTP errors, schema headers and binary bytes", async () => {
    host(({ id }) => ({
      id,
      result: {
        status: 409,
        headers: { "X-Studio-API-Schema-Mismatch": "1" },
        base64: "YQD/",
      },
    }));
    const response = await nativeFetch(
      new Request(origin + "/api/state"),
      origin,
    );
    expect(response.status).toBe(409);
    expect(response.headers.get("X-Studio-API-Schema-Mismatch")).toBe("1");
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(
      new Uint8Array([97, 0, 255]),
    );
  });
  it("carries caller operation identity and explicit POST even with a null body", async () => {
    const invoke = host(({ id, params }) => {
      expect(params).toMatchObject({
        method: "POST",
        body: null,
        requestId: "stable-operation",
        apiSchema: "client-hash",
        workspaceId: "workspace",
        httpResponse: true,
      });
      expect(params).not.toHaveProperty("token");
      return { id, result: { status: 204, headers: {}, base64: "" } };
    });
    const response = await nativeFetch(
      new Request(origin + "/api/messages", {
        method: "POST",
        body: "null",
        headers: {
          "X-Studio-Request-Id": "stable-operation",
          "X-Studio-API-Schema": "client-hash",
          "X-Canvas-Workspace": "workspace",
          "X-Canvas-Token": "renderer-token",
        },
      }),
      origin,
    );
    expect(response.status).toBe(204);
    expect(await response.text()).toBe("");
    expect(invoke).toHaveBeenCalledTimes(1);
  });
  it("rejects external paths and pre-aborted commands without invoking Rust", async () => {
    const invoke = host(() => {
      throw new Error("unexpected call");
    });
    for (const url of [
      "https://other.test/api/state",
      origin + "/secret",
      origin + "/api/state#fragment",
    ])
      await expect(nativeFetch(new Request(url), origin)).rejects.toThrow(
        "server API",
      );
    await expect(
      nativeFetch(
        new Request(origin + "/api/messages", {
          method: "POST",
          signal: AbortSignal.abort(),
        }),
        origin,
      ),
    ).rejects.toMatchObject({ name: "AbortError" });
    expect(invoke).not.toHaveBeenCalled();
  });
  it("aborts only the wait and never retries a submitted mutation", async () => {
    const controller = new AbortController();
    let finish: (() => void) | undefined;
    const invoke = host(
      ({ id }) =>
        new Promise((resolve) => {
          finish = () =>
            resolve({ id, result: { status: 204, headers: {}, base64: "" } });
          controller.abort();
        }),
    );
    await expect(
      nativeFetch(
        new Request(origin + "/api/messages", {
          method: "POST",
          body: "{}",
          signal: controller.signal,
        }),
        origin,
      ),
    ).rejects.toMatchObject({ name: "AbortError" });
    finish?.();
    expect(invoke).toHaveBeenCalledTimes(1);
  });
  it("returns a 304 response with an empty body", async () => {
    host(({ id }) => ({
      id,
      result: { status: 304, headers: { etag: '"rev"' }, base64: "" },
    }));
    const response = await nativeFetch(
      new Request(origin + "/api/state"),
      origin,
    );
    expect(response.status).toBe(304);
    expect(response.headers.get("etag")).toBe('"rev"');
    expect(await response.text()).toBe("");
  });
});
