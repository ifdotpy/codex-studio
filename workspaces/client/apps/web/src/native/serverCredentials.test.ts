import { afterEach, expect, it, vi } from "vitest";
vi.mock("./client", () => ({ nativeRequest: vi.fn() }));
import { nativeRequest } from "./client";
import { NativeServerCredentials } from "./serverCredentials";
const server = {
  id: "remote",
  label: "Remote",
  origin: "https://remote.tailnet.ts.net",
  credentialId: "client",
};
afterEach(() => vi.resetAllMocks());
it("preserves request identity and exact bytes through native response chunks", async () => {
  const invoke = vi.mocked(nativeRequest);
  invoke
    .mockResolvedValueOnce({
      status: 409,
      headers: [["content-type", "application/octet-stream"]],
      stream: "s",
    })
    .mockResolvedValueOnce({ base64: "YQD/" })
    .mockResolvedValueOnce({ done: true });
  const response = await new NativeServerCredentials().fetch(
    server,
    new Request(server.origin + "/api/test", {
      method: "POST",
      body: "null",
      headers: { "X-Studio-Request-Id": "original" },
    }),
  );
  expect(response.status).toBe(409);
  expect([...new Uint8Array(await response.arrayBuffer())]).toEqual([
    97, 0, 255,
  ]);
  expect(invoke.mock.calls[0]).toEqual([
    "servers.request",
    expect.objectContaining({
      serverId: "remote",
      credentialId: "client",
      requestId: "original",
      base64: "bnVsbA==",
    }),
  ]);
});
it("cancels the native stream when the caller aborts", async () => {
  const abort = new AbortController();
  const invoke = vi.mocked(nativeRequest);
  invoke.mockImplementation(async (method) =>
    method === "servers.request"
      ? { status: 200, headers: [], stream: "s" }
      : method === "servers.read"
        ? new Promise(() => {})
        : undefined,
  );
  const response = await new NativeServerCredentials().fetch(
    server,
    new Request(server.origin + "/api/test", { signal: abort.signal }),
  );
  abort.abort();
  await expect(response.text()).rejects.toThrow();
  expect(invoke).toHaveBeenCalledWith("servers.cancel", { stream: "s" });
});
it("does not retry an uncertain server write", async () => {
  vi.mocked(nativeRequest).mockRejectedValue(new Error("lost response"));
  await expect(
    new NativeServerCredentials().fetch(
      server,
      new Request(server.origin + "/api/test", {
        method: "POST",
        body: "null",
        headers: { "X-Studio-Request-Id": "original" },
      }),
    ),
  ).rejects.toThrow("lost response");
  expect(nativeRequest).toHaveBeenCalledOnce();
});
it("releases every rejected summary response before the next poll", async () => {
  const { setServerCredentialAdapter } = await import("../servers/transport");
  const { fetchServerSummary, SchemaMismatchError } =
    await import("../servers/summary");
  vi.stubGlobal("window", {});
  const open = new Set<string>();
  let next = 0;
  vi.mocked(nativeRequest).mockImplementation(async (method, params) => {
    if (method === "servers.request") {
      if (open.size >= 32) throw new Error("Too many open server responses.");
      const stream = String(++next);
      open.add(stream);
      return {
        status: 200,
        headers: [["x-studio-api-schema", "other"]],
        stream,
      };
    }
    if (method === "servers.read") return { base64: "e30=" };
    if (method === "servers.cancel")
      open.delete((params as { stream: string }).stream);
  });
  setServerCredentialAdapter(new NativeServerCredentials());
  try {
    for (let index = 0; index < 40; index++) {
      await expect(
        fetchServerSummary(server, new AbortController().signal),
      ).rejects.toBeInstanceOf(SchemaMismatchError);
      expect(open.size).toBe(0);
    }
  } finally {
    vi.unstubAllGlobals();
  }
});
