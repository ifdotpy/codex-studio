import type { components } from "../../src/generated/api.ts";

export const initializeInstrumentation = ({
  stateDir,
  id,
}: {
  stateDir: string | null | undefined;
  id: string;
}) => {
  function resourceFrame(
    source: string,
  ): components["schemas"]["ResourceChangeEvent"] {
    // The fixture stream and renderer share the generated resource envelope.
    return JSON.parse(source) as components["schemas"]["ResourceChangeEvent"];
  }
  localStorage.setItem(`codex-desktop-opened:${stateDir}`, JSON.stringify(id));
  window.prefetchUiResourceFlushAcks = [];
  window.prefetchUiResourceFlushObserved = [];
  window.prefetchUiPendingResourceFlushRevisions = [];
  window.prefetchUiActiveResourceFlushRevisions = [];
  const browserWindow: Window = window;
  const nativeSetTimeout = browserWindow.setTimeout.bind(browserWindow);
  browserWindow.setTimeout = function (
    callback: TimerHandler,
    delay?: number,
    ...args: unknown[]
  ) {
    if (
      typeof callback === "function" &&
      callback.name === "flushResourceChanges"
    ) {
      const revisions =
        window.prefetchUiPendingResourceFlushRevisions.splice(0);
      window.prefetchUiResourceFlushObserved.push(revisions);
      return nativeSetTimeout(() => {
        window.prefetchUiActiveResourceFlushRevisions = revisions;
        try {
          callback(...args);
        } finally {
          window.prefetchUiActiveResourceFlushRevisions = [];
        }
      }, delay);
    }
    if (delay === 0 && window.prefetchUiActiveResourceFlushRevisions.length > 0)
      window.prefetchUiResourceFlushAcks.push(
        ...window.prefetchUiActiveResourceFlushRevisions,
      );
    return nativeSetTimeout(callback, delay, ...args);
  };
  const addEventListener = EventSource.prototype.addEventListener;
  EventSource.prototype.addEventListener = function (
    type: string,
    listener:
      | EventListenerOrEventListenerObject
      | ((this: EventSource, event: MessageEvent) => void),
    options?: boolean | AddEventListenerOptions,
  ) {
    if (type !== "resources" || typeof listener !== "function") {
      const add = addEventListener.bind(this);
      if (typeof listener === "function") return add(type, listener, options);
      return add(type, listener, options);
    }
    const trackedListener = function (this: EventSource, event: MessageEvent) {
      try {
        window.prefetchUiPendingResourceFlushRevisions.push(
          resourceFrame(event instanceof MessageEvent ? event.data : "null")
            .revision,
        );
      } catch {
        // Ignore malformed resource envelopes in test instrumentation.
      }
      return listener.call(this, event);
    };
    return addEventListener.bind(this)(type, trackedListener, options);
  };
};

declare global {
  interface Window {
    prefetchUiResourceFlushAcks: number[];
    prefetchUiResourceFlushObserved: number[][];
    prefetchUiPendingResourceFlushRevisions: number[];
    prefetchUiActiveResourceFlushRevisions: number[];
    prefetchUiStopUnrelatedWatch: () => void;
    prefetchUiSwitchFrames: {
      time: number;
      fresh: boolean;
      wrong: boolean;
      title: string | undefined;
      titleMatches: boolean;
      count: number;
      scrollTop: number | undefined;
    }[];
    prefetchUiCaptureSwitch: number;
    prefetchUiSwitchStartedAt: number | null;
  }
}
