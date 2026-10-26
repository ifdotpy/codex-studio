import type { PerformanceTranscriptItem as Message } from "./performance-fixture.ts";
export function installResponsivenessFixture({
  stateDir,
  id,
  rows,
}: {
  stateDir: string;
  id: string;
  rows: [string, Pick<Message, "id" | "role">[]][];
}) {
  localStorage.setItem(`codex-desktop-opened:${stateDir}`, JSON.stringify(id));
  localStorage.setItem(`codex-mobile-opened:${stateDir}`, JSON.stringify(id));
  for (const [chat, items] of rows)
    localStorage.setItem(
      `studio-turns:${stateDir}:${chat}:tools-v3`,
      JSON.stringify(
        Object.fromEntries(
          items
            .filter((item) => item.role === "tool")
            .map((item) => [item.id, false]),
        ),
      ),
    );
  window.fixtureStreams = [];
  const NativeEventSource = window.EventSource;
  window.EventSource = new Proxy(NativeEventSource, {
    construct(target, args) {
      const url = String(args[0]);
      if (new URL(url, location.href).pathname === "/api/sync/stream")
        return Reflect.construct(target, args);
      class TranscriptFixtureStream extends EventTarget {
        declare url: string;
        constructor() {
          super();
          this.url = url;
          window.fixtureStreams.push(this);
        }
        close() {
          window.fixtureStreams = window.fixtureStreams.filter(
            (stream) => stream !== this,
          );
        }
      }
      return new TranscriptFixtureStream();
    },
  });
  window.metrics = {
    phase: "startup",
    inputSequence: 0,
    lastInputAt: -Infinity,
    input: [] as { phase: string; duration: number; value: string }[],
    layoutReads: {} as Record<string, number>,
    longTasks: [] as { phase: string; start: number; duration: number }[],
    events: [] as {
      phase: string;
      type: string;
      duration: number;
      inputDelay: number;
    }[],
    renderProbe: [] as {
      component: string;
      phase: string;
      inputSequence: number;
      inputAgeMs: number;
    }[],
    switches: [] as { phase: string; duration: number; expected: string }[],
    stream: [] as { phase: string; revision: number; duration: number }[],
  };
  window.__studioPromptComposerRenderProbe = (component) =>
    window.metrics.renderProbe.push({
      component,
      phase: window.metrics.phase,
      inputSequence: window.metrics.inputSequence,
      inputAgeMs: performance.now() - window.metrics.lastInputAt,
    });
  document.addEventListener(
    "input",
    () => {
      window.metrics.inputSequence++;
      window.metrics.lastInputAt = performance.now();
    },
    true,
  );
  const rectangle = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (...args) {
    const key = `${window.metrics.phase}:${this.tagName}`;
    window.metrics.layoutReads[key] =
      (window.metrics.layoutReads[key] || 0) + 1;
    return rectangle.apply(this, args);
  };
  new PerformanceObserver((list) =>
    window.metrics.longTasks.push(
      ...list.getEntries().map((entry) => ({
        phase: window.metrics.phase,
        start: entry.startTime,
        duration: entry.duration,
      })),
    ),
  ).observe({ type: "longtask", buffered: true });
  new PerformanceObserver((list) =>
    window.metrics.events.push(
      ...list.getEntries().map((entry) => ({
        phase: window.metrics.phase,
        type: entry.name,
        duration: entry.duration,
        inputDelay:
          (entry as PerformanceEventTiming).processingStart - entry.startTime,
      })),
    ),
  ).observe({
    type: "event",
    buffered: true,
    durationThreshold: 16,
  } as PerformanceObserverInit & { durationThreshold: number });
  document.addEventListener(
    "input",
    (event) => {
      if ((event.target as HTMLTextAreaElement).id !== "message") return;
      const started = event.timeStamp,
        value = (event.target as HTMLTextAreaElement).value;
      requestAnimationFrame(() =>
        requestAnimationFrame(() =>
          window.metrics.input.push({
            phase: window.metrics.phase,
            duration: performance.now() - started,
            value,
          }),
        ),
      );
    },
    true,
  );
  document.addEventListener(
    "pointerdown",
    (event) => {
      const chat = (event.target as Element).closest("[data-chat]");
      if (!chat || !window.switchExpected) return;
      const started = event.timeStamp,
        expected = window.switchExpected;
      const inspect = () => {
        if (!document.querySelector(`[data-message="${expected}"]`))
          return requestAnimationFrame(inspect);
        requestAnimationFrame(() =>
          window.metrics.switches.push({
            phase: window.metrics.phase,
            duration: performance.now() - started,
            expected,
          }),
        );
      };
      requestAnimationFrame(inspect);
    },
    true,
  );
  window.emitFixture = async (id: string, revision: number) => {
    const started = performance.now();
    await window.__notifyTranscriptFixture(id);
    await new Promise<void>((resolve) => {
      const inspect = () => {
        if (
          !document
            .querySelector('[data-message="a-live"]')
            ?.textContent!.includes(`Stream revision ${revision}.`)
        )
          return requestAnimationFrame(inspect);
        requestAnimationFrame(() => {
          window.metrics.stream.push({
            phase: window.metrics.phase,
            revision,
            duration: performance.now() - started,
          });
          resolve();
        });
      };
      requestAnimationFrame(inspect);
    });
  };
}
declare global {
  interface Window {
    fixtureStreams: (EventTarget & {
      url: string;
      close: () => void;
    })[];
    metrics: ResponsivenessMetrics;
    switchExpected: string;
    emitFixture: (id: string, revision: number) => Promise<void>;
    __notifyTranscriptFixture: (id: string) => Promise<void>;
  }
}

type ResponsivenessMetrics = {
  phase: string;
  inputSequence: number;
  lastInputAt: number;
  input: { phase: string; duration: number; value: string }[];
  layoutReads: Record<string, number>;
  longTasks: { phase: string; start: number; duration: number }[];
  events: {
    phase: string;
    type: string;
    duration: number;
    inputDelay: number;
  }[];
  renderProbe: {
    component: string;
    phase: string;
    inputSequence: number;
    inputAgeMs: number;
  }[];
  switches: { phase: string; duration: number; expected: string }[];
  stream: { phase: string; revision: number; duration: number }[];
};
