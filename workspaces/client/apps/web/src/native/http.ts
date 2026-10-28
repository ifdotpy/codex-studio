import { localCore, type CoreClient } from "./client";
import { requestIdentity } from "../servers/pairing";

import type { HttpResponse } from "./host";

// Aborting stops the caller's wait. A submitted mutation may already have
// committed; never retry it or claim that abort rolled it back.
function waitFor<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const abort = () => reject(signal.reason);
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) abort();
    work
      .then(resolve, reject)
      .finally(() => signal.removeEventListener("abort", abort));
  });
}

/** One API request through the client core of the server at `expectedOrigin`. */
export async function nativeFetch(
  request: Request,
  expectedOrigin: string,
  core: CoreClient = localCore,
): Promise<Response> {
  request.signal.throwIfAborted();
  const url = new URL(request.url);
  if (
    url.origin !== expectedOrigin ||
    url.username ||
    url.password ||
    url.hash ||
    !url.pathname.startsWith("/api/")
  )
    throw new Error("The request does not belong to this server API.");
  if (request.method !== "GET" && request.method !== "POST")
    throw new Error("The native API accepts only GET and POST requests.");
  if (request.headers.get("Accept") === "text/event-stream")
    throw new Error("Native event streams use NativeEventSource.");
  // A paired server checks the signed request identity. A write keeps the
  // identity of its body, so a resend after a lost reply is the same request.
  const requestId =
    request.headers.get("X-Studio-Request-Id") ??
    (core.server !== "local" && request.method === "POST"
      ? await waitFor(requestIdentity(request), request.signal)
      : undefined);
  let body: unknown;
  if (request.method === "POST") {
    const text = await waitFor(request.text(), request.signal);
    body = text ? JSON.parse(text) : null;
  }
  request.signal.throwIfAborted();
  const reply = await waitFor(
    core.request<HttpResponse>("api.request", {
      path: url.pathname + url.search,
      method: request.method,
      body,
      requestId,
      apiSchema: request.headers.get("X-Studio-API-Schema") ?? undefined,
      workspaceId: request.headers.get("X-Canvas-Workspace") ?? undefined,
      etag: request.headers.get("If-None-Match") ?? undefined,
      httpResponse: true,
    }),
    request.signal,
  );
  const bytes = Uint8Array.from(atob(reply.base64), (character) =>
    character.charCodeAt(0),
  );
  return new Response([204, 205, 304].includes(reply.status) ? null : bytes, {
    status: reply.status,
    headers: reply.headers,
  });
}
