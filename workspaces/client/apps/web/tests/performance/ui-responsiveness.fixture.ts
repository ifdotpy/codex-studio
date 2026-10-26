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
  window.responsivenessFixtureStreams = [];
  const NativeEventSource = window.EventSource;
  window.EventSource = new Proxy(NativeEventSource, {
    construct(target, args) {
      const url = String(args[0]);
      if (new URL(url, location.href).pathname === "/api/sync/stream")
        return Reflect.construct(target, args);
      class TranscriptFixtureStream extends EventTarget {
        url: string;
        constructor() {
          super();
          this.url = url;
          window.responsivenessFixtureStreams.push(this);
        }
        close() {
          window.responsivenessFixtureStreams =
            window.responsivenessFixtureStreams.filter(
              (stream) => stream !== this,
            );
        }
      }
      return new TranscriptFixtureStream();
    },
  });
  const metrics = {
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
  window.responsivenessMetrics = metrics;
  window.__studioPromptComposerRenderProbe = (component) =>
    window.responsivenessMetrics.renderProbe.push({
      component,
      phase: window.responsivenessMetrics.phase,
      inputSequence: window.responsivenessMetrics.inputSequence,
      inputAgeMs: performance.now() - window.responsivenessMetrics.lastInputAt,
    });
  document.addEventListener(
    "input",
    () => {
      window.responsivenessMetrics.inputSequence++;
      window.responsivenessMetrics.lastInputAt = performance.now();
    },
    true,
  );
  const rectangle = Element.prototype.getBoundingClientRect;
  Element.prototype.getBoundingClientRect = function (...args) {
    const key = `${window.responsivenessMetrics.phase}:${this.tagName}`;
    window.responsivenessMetrics.layoutReads[key] =
      (window.responsivenessMetrics.layoutReads[key] || 0) + 1;
    return rectangle.apply(this, args);
  };
  new PerformanceObserver((list) =>
    window.responsivenessMetrics.longTasks.push(
      ...list.getEntries().map((entry) => ({
        phase: window.responsivenessMetrics.phase,
        start: entry.startTime,
        duration: entry.duration,
      })),
    ),
  ).observe({ type: "longtask", buffered: true });
  const eventOptions = { type: "event", buffered: true, durationThreshold: 16 };
  new PerformanceObserver((list) =>
    window.responsivenessMetrics.events.push(
      ...list.getEntries().map((entry) => ({
        phase: window.responsivenessMetrics.phase,
        type: entry.name,
        duration: entry.duration,
        inputDelay:
          ("processingStart" in entry &&
          typeof entry.processingStart === "number"
            ? entry.processingStart
            : 0) - entry.startTime,
      })),
    ),
  ).observe(eventOptions);
  document.addEventListener(
    "input",
    (event) => {
      if (
        !(event.target instanceof HTMLTextAreaElement) ||
        event.target.id !== "message"
      )
        return;
      const started = event.timeStamp,
        value = event.target.value;
      requestAnimationFrame(() =>
        requestAnimationFrame(() =>
          window.responsivenessMetrics.input.push({
            phase: window.responsivenessMetrics.phase,
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
      const chat =
        event.target instanceof Element && event.target.closest("[data-chat]");
      if (!chat || !window.responsivenessSwitchExpected) return;
      const started = event.timeStamp,
        expected = window.responsivenessSwitchExpected;
      const inspect = () => {
        if (!document.querySelector(`[data-message="${expected}"]`))
          return requestAnimationFrame(inspect);
        requestAnimationFrame(() =>
          window.responsivenessMetrics.switches.push({
            phase: window.responsivenessMetrics.phase,
            duration: performance.now() - started,
            expected,
          }),
        );
      };
      requestAnimationFrame(inspect);
    },
    true,
  );
  window.responsivenessEmitFixture = async (id: string, revision: number) => {
    const started = performance.now();
    await window.__notifyTranscriptFixture(id);
    await new Promise<void>((resolve) => {
      const inspect = () => {
        if (
          !document
            .querySelector('[data-message="a-live"]')
            ?.textContent?.includes(`Stream revision ${revision}.`)
        )
          return requestAnimationFrame(inspect);
        requestAnimationFrame(() => {
          window.responsivenessMetrics.stream.push({
            phase: window.responsivenessMetrics.phase,
            revision,
            duration: performance.now() - started,
          });
          resolve();
        });
      };
      requestAnimationFrame(inspect);
    });
  };
  return metrics;
}
declare global {
  interface Window {
    responsivenessFixtureStreams: (EventTarget & {
      url: string;
      close: () => void;
    })[];
    responsivenessMetrics: ReturnType<typeof installResponsivenessFixture>;
    responsivenessSwitchExpected: string;
    responsivenessEmitFixture: (id: string, revision: number) => Promise<void>;
    __notifyTranscriptFixture: (id: string) => Promise<void>;
  }
}
