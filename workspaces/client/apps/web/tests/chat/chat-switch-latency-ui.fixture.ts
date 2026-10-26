import type { components } from "../../src/generated/api.ts";

export const initializeInstrumentation = () => {
  function resourceFrame(
    source: string,
  ): components["schemas"]["ResourceChangeEvent"] {
    // The fixture stream and renderer share the generated resource envelope.
    return JSON.parse(source) as components["schemas"]["ResourceChangeEvent"];
  }
  window.__resourceFrames = [];
  window.__chatSwitchPrepareStartedAt = undefined;
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
          window.__resourceFrames.push({
            ...resourceFrame((event as MessageEvent).data),
            receivedAt: performance.now(),
          });
        } catch {
          /* Ignore non-resource handshake frames. */
        }
      };
      addEventListener.call(this, type, observe, options);
    }
    return addEventListener.call(
      this,
      type,
      listener as EventListenerOrEventListenerObject,
      options,
    );
  };
  window.__chatSwitchStartedAt = undefined;
  window.__chatSwitchFirstRowsPaintAt = undefined;
  window.__chatSwitchPaintAt = undefined;
  window.__chatSwitchScrollStartedAt = undefined;
  window.__chatSwitchLongTasks = [];
  window.__firstSidebarRowVisibleAt = undefined;
  const sidebarObserver = new MutationObserver(() => {
    const row = document.querySelector("[data-chat]");
    if (row?.getClientRects().length) {
      window.__firstSidebarRowVisibleAt = performance.now();
      sidebarObserver.disconnect();
    }
  });
  sidebarObserver.observe(document, { childList: true, subtree: true });
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries())
      window.__chatSwitchLongTasks.push({
        startTime: entry.startTime,
        duration: entry.duration,
      });
  }).observe({ type: "longtask", buffered: true });
};

declare global {
  interface Window {
    __resourceFrames: (components["schemas"]["ResourceChangeEvent"] & {
      receivedAt: number;
    })[];
    __chatSwitchPrepareStartedAt: number | undefined;
    __chatSwitchStartedAt: number | undefined;
    __chatSwitchFirstRowsPaintAt: number | undefined;
    __chatSwitchPaintAt: number | undefined;
    __chatSwitchScrollStartedAt: number | undefined;
    __chatSwitchLongTasks: Pick<PerformanceEntry, "startTime" | "duration">[];
    __firstSidebarRowVisibleAt: number | undefined;
  }
}
