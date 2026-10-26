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
  window.__resourceFlushAcks = [];
  window.__resourceFlushObserved = [];
  window.__pendingResourceFlushRevisions = [];
  window.__activeResourceFlushRevisions = [];
  const nativeSetTimeout = (window as Window).setTimeout.bind(window);
  (window as Window).setTimeout = function (
    callback: TimerHandler,
    delay?: number,
    ...args: unknown[]
  ) {
    if (
      typeof callback === "function" &&
      callback.name === "flushResourceChanges"
    ) {
      const revisions = window.__pendingResourceFlushRevisions.splice(0);
      window.__resourceFlushObserved.push(revisions);
      return nativeSetTimeout(() => {
        window.__activeResourceFlushRevisions = revisions;
        try {
          callback(...args);
        } finally {
          window.__activeResourceFlushRevisions = [];
        }
      }, delay);
    }
    if (delay === 0 && window.__activeResourceFlushRevisions.length > 0)
      window.__resourceFlushAcks.push(...window.__activeResourceFlushRevisions);
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
      return addEventListener.call(
        this,
        type,
        listener as EventListenerOrEventListenerObject,
        options,
      );
    }
    const trackedListener = function (this: EventSource, event: MessageEvent) {
      try {
        window.__pendingResourceFlushRevisions.push(
          resourceFrame(event.data).revision,
        );
      } catch {
        // Ignore malformed resource envelopes in test instrumentation.
      }
      return listener.call(this, event);
    };
    return addEventListener.call(
      this,
      type,
      trackedListener as EventListener,
      options,
    );
  };
};

declare global {
  interface Window {
    __resourceFlushAcks: number[];
    __resourceFlushObserved: number[][];
    __pendingResourceFlushRevisions: number[];
    __activeResourceFlushRevisions: number[];
    stopUnrelatedWatch: () => void;
    switchFrames: {
      time: number;
      fresh: boolean;
      wrong: boolean;
      title: string | undefined;
      titleMatches: boolean;
      count: number;
      scrollTop: number | undefined;
    }[];
    captureSwitch: number;
    switchStartedAt: number | null;
  }
}
