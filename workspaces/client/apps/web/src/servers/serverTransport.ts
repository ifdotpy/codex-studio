import type { StudioServer } from "./registry";

/** An immutable server owner, shared by a view and its background requests. */
export function createServerTransport(
  server: StudioServer,
  fetchOwned: (server: StudioServer, request: Request) => Promise<Response>,
) {
  const owner = Object.freeze({ ...server });
  const lifetime = new AbortController();
  return {
    owner,
    close() {
      lifetime.abort(
        new DOMException("The server view was closed.", "AbortError"),
      );
    },
    async fetch(request: Request): Promise<Response> {
      lifetime.signal.throwIfAborted();
      const url = new URL(request.url);
      if (
        url.origin !== owner.origin ||
        url.username ||
        url.password ||
        url.hash ||
        !url.pathname.startsWith("/api/")
      )
        throw new Error("The request does not belong to this server API.");
      const signal = AbortSignal.any([lifetime.signal, request.signal]);
      signal.throwIfAborted();
      // Closing a view cancels observation, not a mutation that may have reached
      // the server. Preserve its request identity and never submit it again.
      const result = await fetchOwned(owner, new Request(request, { signal }));
      signal.throwIfAborted();
      return result;
    },
  };
}
