import type { components } from "../../src/generated/api.ts";

export const initializeInstrumentation = () => {
  function resourceFrame(
    source: string,
  ): components["schemas"]["ResourceChangeEvent"] {
    // The fixture stream and renderer share the generated resource envelope.
    return JSON.parse(source) as components["schemas"]["ResourceChangeEvent"];
  }
  window.switchLatencyResourceFrames = [];
  window.switchLatencyPrepareStartedAt = undefined;
  const addEventListener = EventSource.prototype.addEventListener;
  EventSource.prototype.addEventListener = function (
    type: string,
    listener:
      | EventListenerOrEventListenerObject
      | ((this: EventSource, event: MessageEvent) => void),
    options?: boolean | AddEventListenerOptions,
  ) {
    if (type === "resources") {
      const observe = (event: Event) => {
        try {
          window.switchLatencyResourceFrames.push({
            ...resourceFrame(
              event instanceof MessageEvent ? event.data : "null",
            ),
            receivedAt: performance.now(),
          });
        } catch {
          /* Ignore non-resource handshake frames. */
        }
      };
      addEventListener.call(this, type, observe, options);
    }
    {
      const add = addEventListener.bind(this);
      if (typeof listener === "function") return add(type, listener, options);
      return add(type, listener, options);
    }
  };
  window.switchLatencyStartedAt = undefined;
  window.switchLatencyFirstRowsPaintAt = undefined;
  window.switchLatencyPaintAt = undefined;
  window.switchLatencyScrollStartedAt = undefined;
  window.switchLatencyLongTasks = [];
  window.switchLatencyFirstSidebarRowVisibleAt = undefined;
  const sidebarObserver = new MutationObserver(() => {
    const row = document.querySelector("[data-chat]");
    if (row?.getClientRects().length) {
      window.switchLatencyFirstSidebarRowVisibleAt = performance.now();
      sidebarObserver.disconnect();
    }
  });
  sidebarObserver.observe(document, { childList: true, subtree: true });
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries())
      window.switchLatencyLongTasks.push({
        startTime: entry.startTime,
        duration: entry.duration,
      });
  }).observe({ type: "longtask", buffered: true });
};

declare global {
  interface Window {
    switchLatencyResourceFrames: (components["schemas"]["ResourceChangeEvent"] & {
      receivedAt: number;
    })[];
    switchLatencyPrepareStartedAt: number | undefined;
    switchLatencyStartedAt: number | undefined;
    switchLatencyFirstRowsPaintAt: number | undefined;
    switchLatencyPaintAt: number | undefined;
    switchLatencyScrollStartedAt: number | undefined;
    switchLatencyLongTasks: Pick<PerformanceEntry, "startTime" | "duration">[];
    switchLatencyFirstSidebarRowVisibleAt: number | undefined;
  }
}
