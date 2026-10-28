import { randomUUID } from "../uuid";
import { frameRequest, isNativeFrame } from "./frame";
import { queueMarkdown } from "./markdownQueue";
import { nativeHost } from "./host";
import { hasHostedClient, hostedRequest, listenHostedEvents } from "./hosted";
import { serverViewId } from "../servers/environment";
export { nativeHost } from "./host";
export { OutcomeUnknownError } from "./hosted";
/** A client core serves this page: the desktop app, or the server of a browser page. */
export function hasNativeClient() {
  return hasDeviceClient() || hasHostedClient();
}
/**
 * The desktop app serves this page, so device commands (microphone, voice,
 * dialogs, notifications, sign-in windows) work. A hosted core refuses them.
 */
export function hasDeviceClient() {
  return typeof nativeHost()?.core?.invoke === "function" || isNativeFrame();
}
// TODO(attar-sync): Restore Attar's synchronous commit path for local durable
// writes (preferences and drafts), plus synchronous pure computations where
// supported. Tauri invoke requires promises; HTTP/SSE must remain asynchronous.
// A synchronous commit must return only after SQLite commit, never after enqueue.
export async function nativeRequest<T>(
  method: string,
  params: unknown,
  id: string = randomUUID(),
  signal?: AbortSignal,
): Promise<T> {
  if (!id || id.length > 128)
    throw new Error("Invalid native request identity.");
  if (method === "markdown.parse")
    return queueMarkdown(
      () => sendNativeRequest<T>(method, params, id),
      signal,
    );
  return sendNativeRequest<T>(method, params, id);
}

/**
 * `server` names the core of a paired server; without it the command goes to
 * the core of the local server (in a server view frame: of the frame's owner,
 * which the desktop shell derives from the frame).
 */
async function sendNativeRequest<T>(
  method: string,
  params: unknown,
  id: string,
  server?: string,
): Promise<T> {
  if (isNativeFrame()) {
    if (server) throw new Error("A server view reaches only its own server.");
    return frameRequest<T>(method, params, id);
  }
  const native = nativeHost();
  if (!native && !hasHostedClient())
    throw new Error("The native client is unavailable.");
  // Transport errors never cause an automatic retry of a possibly applied command.
  const value = native
    ? await native.core.invoke("client_request", {
        request: { id, method, params },
        ...(server ? { server } : {}),
      })
    : await hostedRequest(id, method, params, server);
  if (
    !value ||
    typeof value !== "object" ||
    !("id" in value) ||
    value.id !== id
  )
    throw new Error("The native client returned a mismatched response.");
  const response = value as { result?: T; error?: { message?: string } };
  if (Object.hasOwn(response, "result") === Object.hasOwn(response, "error"))
    throw new Error("The native client returned an invalid response.");
  if (Object.hasOwn(response, "error"))
    throw new Error(response.error?.message || "The native command failed.");
  return response.result as T;
}

/** The paired server of a core event, or undefined for the local core. */
function eventServer(value: unknown) {
  const server = (value as { server?: unknown } | null)?.server;
  return typeof server === "string" ? server : undefined;
}

export async function listenNativeEvents(
  callback: (value: unknown) => void,
  server?: string,
) {
  if (isNativeFrame()) {
    if (server) throw new Error("A server view reaches only its own server.");
    const receive = (event: MessageEvent) => {
      if (
        event.source !== window.parent ||
        event.origin !== "tauri://localhost"
      )
        return;
      if (event.data?.kind === "studio-native-audio-event")
        callback(event.data);
      // The shell forwards the events of this frame's own server core.
      else if (event.data?.kind === "studio-native-core-event")
        callback(event.data.value);
    };
    window.addEventListener("message", receive);
    return () => window.removeEventListener("message", receive);
  }
  const native = nativeHost();
  if (!native && hasHostedClient()) return listenHostedEvents(callback, server);
  if (!native?.event) throw new Error("Native events are unavailable.");
  // The desktop marks the events of a paired server core with its server id.
  return native.event.listen("studio-client-event", (event) => {
    if (eventServer(event.payload) === server) callback(event.payload);
  });
}

/** The client core of one server: "local" or the id of a paired server. */
export type CoreClient = {
  readonly server: string;
  request<T>(
    method: string,
    params: unknown,
    id?: string,
    signal?: AbortSignal,
  ): Promise<T>;
  listen(callback: (value: unknown) => void): Promise<() => void>;
};

export const localCore: CoreClient = {
  server: "local",
  request: nativeRequest,
  listen: listenNativeEvents,
};

/**
 * The core of `server`. The page names the server in each request; the
 * desktop or the hosting server keeps the core, its state and its keys. A
 * server view frame reaches only the core of its own server, which the shell
 * selects from the frame.
 */
export function coreForServer(server: string): CoreClient {
  if (server === "local" || (isNativeFrame() && server === serverViewId))
    return localCore;
  return {
    server,
    request: <T>(
      method: string,
      params: unknown,
      id: string = randomUUID(),
    ): Promise<T> => {
      if (!id || id.length > 128)
        throw new Error("Invalid native request identity.");
      return sendNativeRequest<T>(method, params, id, server);
    },
    listen: (callback) => listenNativeEvents(callback, server),
  };
}

/** The core of the server that this page shows. */
export const pageCore = (): CoreClient =>
  coreForServer(serverViewId ?? "local");
