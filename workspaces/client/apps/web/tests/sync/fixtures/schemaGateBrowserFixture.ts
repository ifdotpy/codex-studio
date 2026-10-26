import type { components } from "../../../src/generated/api.ts";
type Handshake = components["schemas"]["ApiSchemaEvent"];
/** Self-contained because Playwright serializes this function into the page. */
export function installSchemaGateFixture(
  options:
    | { mode: "cold"; handshake: Handshake }
    | {
        mode: "session";
        handshakes: { match: Handshake; mismatch: Handshake };
      },
) {
  const behavior =
    options.mode === "cold"
      ? "mismatch"
      : sessionStorage.getItem("schema-gate-handshake");
  if (options.mode === "cold") {
    const nativeFetch = window.fetch.bind(window);
    window.studioSchemaGatePendingApiFetches = 0;
    window.fetch = async (input, init) => {
      const url = new URL(
        input instanceof Request ? input.url : String(input),
        location.href,
      );
      // The preference client retries these endpoints on missing response
      // hashes independently of the stream handshake exercised here.
      const omitSchemaHash =
        url.pathname.startsWith("/api/") &&
        url.pathname !== "/api/sync/identity" &&
        url.pathname !== "/api/sync/pull" &&
        url.pathname !== "/api/session" &&
        url.pathname !== "/api/sync/preferences";
      if (omitSchemaHash) window.studioSchemaGatePendingApiFetches++;
      try {
        const response = await nativeFetch(input, init);
        if (!omitSchemaHash) return response;
        const headers = new Headers(response.headers);
        headers.delete("x-studio-api-schema");
        return new Response(response.body, {
          status: response.status,
          statusText: response.statusText,
          headers,
        });
      } finally {
        if (omitSchemaHash) window.studioSchemaGatePendingApiFetches--;
      }
    };

    window.studioSchemaGateHandshakeCount = 0;
  } else {
    window.__schemaGateStreamOpens = Number(
      sessionStorage.getItem("schema-gate-stream-opens") || "0",
    );
    if (sessionStorage.getItem("schema-gate-strip-api-hash") === "1") {
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const response = await nativeFetch(input, init);
        const url = new URL(
          input instanceof Request ? input.url : String(input),
          location.href,
        );
        if (!url.pathname.startsWith("/api/")) return response;
        if (url.pathname === "/api/sync/identity") return response;
        const headers = new Headers(response.headers);
        headers.delete("x-studio-api-schema");
        if (init?.method === "GET" || !init?.method)
          window.__schemaGateHeaderlessGets =
            (window.__schemaGateHeaderlessGets || 0) + 1;
        return new Response(response.body, {
          status: response.status,
          statusText: response.statusText,
          headers,
        });
      };
    }
  }
  if (behavior !== "match" && behavior !== "mismatch" && behavior !== "silent")
    return;
  const handshake =
    options.mode === "cold"
      ? options.handshake
      : behavior === "silent"
        ? undefined
        : options.handshakes[behavior];
  class TestEventSource extends EventTarget implements EventSource {
    declare addEventListener: EventSource["addEventListener"];
    declare removeEventListener: EventSource["removeEventListener"];
    static CONNECTING = 0 as const;
    readonly CONNECTING = TestEventSource.CONNECTING;
    readonly OPEN = TestEventSource.OPEN;
    readonly CLOSED = TestEventSource.CLOSED;
    static OPEN = 1 as const;
    static CLOSED = 2 as const;
    readyState: number = TestEventSource.CONNECTING;
    withCredentials = false;
    onopen: EventSource["onopen"] = null;
    onmessage: EventSource["onmessage"] = null;
    onerror: EventSource["onerror"] = null;
    url;
    #closed = false;

    constructor(url: string | URL) {
      super();
      this.url = new URL(url, location.href).href;
      queueMicrotask(() => {
        if (this.#closed) return;
        this.readyState = TestEventSource.OPEN;
        if (options.mode === "session") {
          window.__schemaGateStreamOpens++;
          sessionStorage.setItem(
            "schema-gate-stream-opens",
            String(window.__schemaGateStreamOpens),
          );
        }
        this.dispatchEvent(new Event("open"));
        if (behavior === "silent") return;
        this.dispatchEvent(
          new MessageEvent("api-schema", {
            data: JSON.stringify(handshake),
          }),
        );
        if (options.mode === "cold") window.studioSchemaGateHandshakeCount++;
        else if (behavior === "mismatch")
          window.__schemaGateMismatchHandshakes =
            (window.__schemaGateMismatchHandshakes || 0) + 1;
      });
    }

    dispatchEvent(event: Event) {
      const dispatched = super.dispatchEvent(event);
      if (event.type === "open") this.onopen?.call(this, event);
      if (event.type === "error") this.onerror?.call(this, event);
      if (event.type === "message" && event instanceof MessageEvent)
        this.onmessage?.call(this, event);
      return dispatched;
    }

    close() {
      this.#closed = true;
      this.readyState = TestEventSource.CLOSED;
    }
  }

  window.EventSource = TestEventSource;
}

declare global {
  interface Window {
    studioSchemaGatePendingApiFetches: number;
    studioSchemaGateHandshakeCount: number;
    studioTestSources: EventSource[];
    studioSchemaGateOpenStreams: number;
    __originalGetRegistration?: ServiceWorkerContainer["getRegistration"];
    __schemaGateStreamOpens: number;
    __schemaGateHeaderlessGets: number;
    __schemaGateMismatchHandshakes: number;
  }
}
