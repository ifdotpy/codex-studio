import { expect, it, vi } from "vitest";
import type {
  CredentialBridge,
  CredentialAction,
  CredentialRequest,
  CredentialResponse,
  CredentialResult,
  StreamChunk,
} from "../../../desktop/api.ts";
import { DesktopServerCredentials } from "./desktopCredentials";
import type { StudioServer } from "./registry";

const server = {
  id: "remote",
  label: "Remote",
  origin: "https://remote.tailnet.ts.net",
  credentialId: "credential",
} satisfies StudioServer;

function fixture(response: CredentialResponse) {
  const buffered = {
    status: 200,
    statusText: "OK",
    headers: [],
    body: new ArrayBuffer(0),
  } satisfies CredentialResult<"summary">;
  const results: { [A in CredentialAction]: CredentialResult<A> } = {
    pair: server,
    pairApproved: server,
    pairAttempt: false,
    request: response,
    summary: buffered,
    cancel: undefined,
    forget: undefined,
  };
  const action =
    vi.fn<
      (
        request: Pick<CredentialRequest, "action"> &
          Partial<Pick<CredentialRequest<"request">, "streamId">>,
      ) => void
    >();
  const stop = vi.fn();
  let receive: ((chunk: StreamChunk) => void) | undefined;
  const onServerStream = vi.fn<CredentialBridge["onServerStream"]>(
    (callback) => {
      receive = callback;
      return stop;
    },
  );
  const bridge: CredentialBridge = {
    serverCredentialAction: async (request) => {
      action(request);
      return results[request.action];
    },
    onServerStream,
  };
  return {
    action,
    stop,
    onServerStream,
    adapter: new DesktopServerCredentials(bridge),
    receive: (chunk: StreamChunk) => receive?.(chunk),
  };
}

it("carries POST identity and bytes through a credential-only bridge", async () => {
  const response = {
    status: 200,
    statusText: "OK",
    headers: [],
    body: new TextEncoder().encode("accepted").buffer,
  } satisfies CredentialResult<"request">;
  const { action, stop, adapter } = fixture(response);
  const request = new Request(server.origin + "/api/messages", {
    method: "POST",
    body: JSON.stringify({ requestId: "write-once", text: "hello" }),
  });
  expect(await (await adapter.fetch(server, request)).text()).toBe("accepted");
  expect(action).toHaveBeenCalledWith(
    expect.objectContaining({
      action: "request",
      method: "POST",
      requestId: "write-once",
      credentialId: server.credentialId,
      body: await request.clone().arrayBuffer(),
    }),
  );
  expect(stop).toHaveBeenCalledOnce();
});

it("routes stream bytes and cancellation to the original server and stream", async () => {
  const { action, stop, adapter, receive } = fixture({
    status: 200,
    statusText: "OK",
    headers: [],
    streamId: "stream",
  } satisfies CredentialResult<"request">);
  const response = await adapter.fetch(
    server,
    new Request(server.origin + "/api/sync/stream"),
  );
  const [call] = action.mock.calls;
  expect(call?.[0].action).toBe("request");
  const streamId = call?.[0].streamId;
  if (!streamId) throw new Error("Expected the owned stream request.");
  const reader = response.body?.getReader();
  expect(reader).toBeDefined();
  receive({
    serverId: server.id,
    streamId,
    bytes: new TextEncoder().encode("event").buffer,
  });
  expect(new TextDecoder().decode((await reader?.read())?.value)).toBe("event");
  await reader?.cancel();
  expect(action).toHaveBeenLastCalledWith({
    action: "cancel",
    serverId: server.id,
    streamId,
    reason: "frame stopped reading",
    frameOwner: undefined,
  });
  expect(stop).toHaveBeenCalledOnce();
});

it("rejects unsupported methods before opening a stream", async () => {
  const { action, onServerStream, adapter } = fixture({
    status: 200,
    statusText: "OK",
    headers: [],
    body: new ArrayBuffer(0),
  });
  await expect(
    adapter.fetch(
      server,
      new Request(server.origin + "/api/messages", { method: "DELETE" }),
    ),
  ).rejects.toThrow("Unsupported server method.");
  expect(action).not.toHaveBeenCalled();
  expect(onServerStream).not.toHaveBeenCalled();
});
