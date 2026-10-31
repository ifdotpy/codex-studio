/** Instrument the real stream and input without substituting transcript data. */
export function installResponsivenessFixture({
  stateDir,
  id,
  cachedId,
}: {
  stateDir: string;
  id: string;
  cachedId: string;
}) {
  localStorage.setItem(`codex-desktop-opened:${stateDir}`, cachedId);
  localStorage.setItem(`codex-mobile-opened:${stateDir}`, JSON.stringify(id));
  window.metrics = {
    phase: "startup",
    inputSequence: 0,
    lastInputAt: -Infinity,
    input: [],
    renderProbe: [],
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
    (event) => {
      if ((event.target as HTMLTextAreaElement).id !== "message") return;
      window.metrics.inputSequence++;
      window.metrics.lastInputAt = performance.now();
      const started = event.timeStamp;
      const value = (event.target as HTMLTextAreaElement).value;
      const phase = window.metrics.phase;
      requestAnimationFrame(() =>
        requestAnimationFrame(() => {
          window.metrics.input.push({
            phase,
            duration: performance.now() - started,
            value,
          });
        }),
      );
    },
    true,
  );
}

declare global {
  interface Window {
    metrics: {
      phase: string;
      inputSequence: number;
      lastInputAt: number;
      input: { phase: string; duration: number; value: string }[];
      renderProbe: {
        component: string;
        phase: string;
        inputSequence: number;
        inputAgeMs: number;
      }[];
    };
  }
}
