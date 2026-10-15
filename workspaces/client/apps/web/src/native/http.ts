import { nativeEventResponse } from "./stream";
import { nativeRequest } from "./client";

type HttpResponse = {
  status: number;
  headers: Record<string, string>;
  base64: string;
};

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

export async function nativeFetch(
  request: Request,
  expectedOrigin: string,
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
  if (
    request.method === "GET" &&
    request.headers.get("Accept") === "text/event-stream"
  )
    return nativeEventResponse(request, url.pathname + url.search);
  let body: unknown;
  if (request.method === "POST") {
    const text = await waitFor(request.text(), request.signal);
    body = text ? JSON.parse(text) : null;
  }
  request.signal.throwIfAborted();
  const reply = await waitFor(
    nativeRequest<HttpResponse>("api.request", {
      path: url.pathname + url.search,
      method: request.method,
      body,
      requestId: request.headers.get("X-Studio-Request-Id") ?? undefined,
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
