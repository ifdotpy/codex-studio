import type { ResourceChangeEvent } from "../playwright.ts";
import type { components } from "../../src/generated/api.ts";

declare global {
  interface Window {
    __entityFixtureResourceEvents: ResourceChangeEvent[];
    __entityFixtureStreams: Array<{
      url: string;
      open: boolean;
      closeCalls: number;
    }>;
    __entityFixtureSubscriptionAcks: components["schemas"]["SyncSubscriptionAck"][];
  }
}

export function installEntityContractProbe() {
  const nativeFetch = window.fetch.bind(window);
  window.__entityFixtureResourceEvents = [];
  window.__entityFixtureStreams = [];
  window.__entityFixtureSubscriptionAcks = [];
  window.fetch = async (input, init) => {
    const request = new Request(input, init);
    if (new URL(request.url).pathname !== "/api/sync/stream")
      return nativeFetch(request);
    const record = { url: request.url, open: false, closeCalls: 0 };
    window.__entityFixtureStreams.push(record);
    request.signal.addEventListener("abort", () => {
      record.open = false;
      record.closeCalls++;
    });
    const response = await nativeFetch(request);
    record.open = response.ok && !request.signal.aborted;
    const decoder = new TextDecoder();
    let pending = "";
    if (!response.body) throw new Error("Fixture stream has no response body");
    const observed = response.body.pipeThrough(
      new TransformStream<Uint8Array, Uint8Array>({
        transform(chunk, controller) {
          controller.enqueue(chunk);
          pending += decoder.decode(chunk, { stream: true });
          let end;
          while ((end = pending.indexOf("\n\n")) >= 0) {
            const frame = pending.slice(0, end);
            pending = pending.slice(end + 2);
            const name = frame.match(/^event: (.+)$/m)?.[1];
            const data = frame.match(/^data: (.+)$/m)?.[1];
            if (!data) continue;
            if (name === "resources")
              window.__entityFixtureResourceEvents.push(JSON.parse(data));
            if (name === "subscriptions")
              window.__entityFixtureSubscriptionAcks.push(JSON.parse(data));
          }
        },
        flush() {
          record.open = false;
        },
      }),
    );
    return new Response(observed, {
      status: response.status,
      headers: response.headers,
    });
  };
}
