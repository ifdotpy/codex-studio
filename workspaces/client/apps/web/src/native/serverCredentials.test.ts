import { afterEach, expect, it, vi } from "vitest";
const mocks = vi.hoisted(() => ({
  nativeRequest: vi.fn(),
  coreRequest: vi.fn(),
  servers: [] as string[],
}));
vi.mock("./client", () => ({
  nativeRequest: mocks.nativeRequest,
  hasNativeClient: () => true,
  localCore: { server: "local", request: mocks.nativeRequest },
  coreForServer: (server: string) => {
    mocks.servers.push(server);
    return { server, request: mocks.coreRequest, listen: vi.fn() };
  },
}));
import { NativeServerCredentials } from "./serverCredentials";
const server = {
  id: "remote",
  label: "Remote",
  origin: "https://remote.tailnet.ts.net",
  credentialId: "client",
};
afterEach(() => {
  vi.resetAllMocks();
  mocks.servers = [];
});

const reply = (status: number, body: string) => ({
  status,
  headers: { "content-type": "application/octet-stream" },
  base64: btoa(body),
});

it("sends a paired server's request through that server's core with its original identity", async () => {
  mocks.coreRequest.mockResolvedValueOnce(reply(409, "conflict"));
  const response = await new NativeServerCredentials().fetch(
    server,
    new Request(server.origin + "/api/test?x=1", {
      method: "POST",
      body: "null",
      headers: { "X-Studio-Request-Id": "original" },
    }),
  );
  expect(response.status).toBe(409);
  expect(await response.text()).toBe("conflict");
  expect(mocks.servers).toEqual(["remote"]);
  expect(mocks.nativeRequest).not.toHaveBeenCalled();
  expect(mocks.coreRequest).toHaveBeenCalledWith(
    "api.request",
    expect.objectContaining({
      path: "/api/test?x=1",
      method: "POST",
      body: null,
      requestId: "original",
    }),
  );
});

it("keeps the identity of a write body for the signed request", async () => {
  mocks.coreRequest.mockResolvedValueOnce(reply(200, "{}"));
  await new NativeServerCredentials().fetch(
    server,
    new Request(server.origin + "/api/messages", {
      method: "POST",
      body: JSON.stringify({ id: "message-one", text: "hi" }),
    }),
  );
  expect(mocks.coreRequest.mock.lastCall?.[1]).toMatchObject({
    requestId: "message-one",
  });
});

it("does not retry an uncertain server write", async () => {
  mocks.coreRequest.mockRejectedValue(new Error("lost response"));
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
  expect(mocks.coreRequest).toHaveBeenCalledOnce();
});

it("pairs and forgets in the key store of this device, never a server core", async () => {
  mocks.nativeRequest.mockResolvedValueOnce(true);
  await expect(
    new NativeServerCredentials().hasPairAttempt({
      requestId: "r",
      serverId: "remote",
      origin: server.origin,
    }),
  ).resolves.toBe(true);
  mocks.nativeRequest.mockResolvedValueOnce(null);
  await new NativeServerCredentials().forget(server);
  expect(mocks.nativeRequest.mock.calls.map((call) => call[0])).toEqual([
    "servers.pairAttempt",
    "servers.forget",
  ]);
  expect(mocks.coreRequest).not.toHaveBeenCalled();
});
