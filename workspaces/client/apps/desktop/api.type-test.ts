import type {
  CredentialRequest,
  CredentialResponse,
  DesktopBridge,
  CredentialBridge,
  NativeRequest,
  NativeResult,
  NativeInvoke,
  PairReceipt,
  BufferedCredentialResponse,
  StreamChunk,
} from "./api.ts";

const unsupportedMethod: CredentialRequest = {
  action: "summary",
  serverId: "local",
  url: "http://localhost/api/ui-summary",
  // @ts-expect-error Only GET, POST and HEAD are supported by the signed adapter.
  method: "DELETE",
};
// @ts-expect-error Buffered and streamed responses are exclusive.
const mixedResponse: CredentialResponse = {
  status: 200,
  statusText: "OK",
  headers: [],
  body: new ArrayBuffer(0),
  streamId: "stream",
};
// @ts-expect-error A response must carry its body or stream identity.
const emptyResponse: CredentialResponse = {
  status: 200,
  statusText: "OK",
  headers: [],
};
// @ts-expect-error A stream message must carry bytes, completion or an error.
const emptyChunk: StreamChunk = { serverId: "local", streamId: "stream" };
// @ts-expect-error Bytes and completion cannot be sent together.
const mixedChunk: StreamChunk = {
  serverId: "local",
  streamId: "stream",
  bytes: new ArrayBuffer(0),
  done: true,
};
const incompleteChunk: StreamChunk = {
  serverId: "local",
  streamId: "stream",
  // @ts-expect-error Completion is always true.
  done: false,
};

function credentialResults(
  bridge: Required<Pick<DesktopBridge, "serverCredentialAction">>,
) {
  const paired: Promise<boolean> = bridge.serverCredentialAction({
    action: "pairAttempt",
    serverId: "remote",
    origin: "https://remote.tailnet.ts.net",
    requestId: "attempt",
  });
  const receipt: Promise<PairReceipt> = bridge.serverCredentialAction({
    action: "pair",
    origin: "https://remote.tailnet.ts.net",
    invitation: {},
    requestId: "attempt",
  });
  const summary: Promise<BufferedCredentialResponse> =
    bridge.serverCredentialAction({
      action: "summary",
      serverId: "local",
      method: "GET",
      url: "http://localhost/api/ui-summary",
    });
  // @ts-expect-error Pair-attempt results are booleans, not buffered responses.
  const wrongResult: Promise<CredentialResponse> =
    bridge.serverCredentialAction({
      action: "pairAttempt",
      serverId: "remote",
      origin: "https://remote.tailnet.ts.net",
      requestId: "attempt",
    });
  // @ts-expect-error Unknown credential actions cannot cross the bridge.
  bridge.serverCredentialAction({ action: "invented" });
  return { paired, receipt, summary, wrongResult };
}
void [
  unsupportedMethod,
  mixedResponse,
  emptyResponse,
  emptyChunk,
  mixedChunk,
  incompleteChunk,
  credentialResults,
];

// @ts-expect-error Transcription requires its payload.
const missingPayload: NativeRequest = { method: "transcribeAudio" };
// @ts-expect-error Native method names are closed.
const unknownMethod: NativeRequest = { method: "invented" };
const wrongPayload: NativeRequest = {
  method: "openExternal",
  // @ts-expect-error External links take a URL, not a file-action payload.
  value: { action: "reveal", target: { path: "/fixture" } },
};
// @ts-expect-error The directory picker returns a path or null.
const wrongNativeResult: NativeResult<"pickDirectory"> = true;
function nativeResults(
  invoke: NativeInvoke,
  bridge: DesktopBridge,
  credentials: CredentialBridge,
) {
  const path: Promise<string | null> = invoke(["pickDirectory"]);
  const attempt: Promise<boolean> = invoke([
    "serverCredentialAction",
    {
      action: "pairAttempt",
      serverId: "remote",
      origin: "https://remote.tailnet.ts.net",
      requestId: "attempt",
    },
  ]);
  // @ts-expect-error A credential-only bridge is not the full native bridge.
  const fullBridge: DesktopBridge = credentials;
  // @ts-expect-error Preload calls require the method's argument.
  invoke(["transcribeAudio"]);
  // @ts-expect-error Preload calls keep method and payload paired.
  invoke(["openExternal", { action: "reveal", target: {} }]);
  // @ts-expect-error Server wrappers cannot expose vault keys.
  bridge.serverNativeAction?.("local", "showVaultKeys", "prefix");
  // @ts-expect-error Wrapped transcription requires its payload too.
  bridge.serverNativeAction?.("remote", "transcribeAudio");
  // @ts-expect-error Wrapped payloads keep their method.
  bridge.serverNativeAction?.("remote", "openExternal", {
    action: "reveal",
    target: {},
  });
  return { path, attempt, fullBridge };
}
void [
  missingPayload,
  unknownMethod,
  wrongPayload,
  wrongNativeResult,
  nativeResults,
];
