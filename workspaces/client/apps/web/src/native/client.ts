import { randomUUID } from "../uuid";
import { frameRequest, isNativeFrame } from "./frame";
import { queueMarkdown } from "./markdownQueue";
type Invoke = (
  command: string,
  args: Record<string, unknown>,
) => Promise<unknown>;
type Host = {
  core: { invoke: Invoke };
  event?: {
    listen: (
      name: string,
      callback: (event: { payload: unknown }) => void,
    ) => Promise<() => void>;
  };
};
export function nativeHost(): Host | undefined {
  return (
    globalThis as typeof globalThis & {
      __TAURI__?: Host;
    }
  ).__TAURI__;
}
export function hasNativeClient() {
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

async function sendNativeRequest<T>(
  method: string,
  params: unknown,
  id: string,
): Promise<T> {
  if (isNativeFrame()) return frameRequest<T>(method, params, id);
  const native = nativeHost();
  if (!native) throw new Error("The native client is unavailable.");
  // Transport errors never cause an automatic retry of a possibly applied command.
  const value = await native.core.invoke("client_request", {
    request: { id, method, params },
  });
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

export async function listenNativeEvents(callback: (value: unknown) => void) {
  if (isNativeFrame()) {
    const receive = (event: MessageEvent) => {
      if (
        event.source === window.parent &&
        event.origin === "tauri://localhost" &&
        event.data?.kind === "studio-native-audio-event"
      )
        callback(event.data);
    };
    window.addEventListener("message", receive);
    return () => window.removeEventListener("message", receive);
  }
  const native = nativeHost();
  if (!native?.event) throw new Error("Native events are unavailable.");
  return native.event.listen("studio-client-event", (event) =>
    callback(event.payload),
  );
}
