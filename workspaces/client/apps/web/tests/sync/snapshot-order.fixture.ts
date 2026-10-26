import type { ResourceChangeEvent } from "../playwright.ts";

type Bounds = { y: number; width: number; height: number; className: string };
type GeometryFrame = {
  phase: string;
  label: Bounds;
  fields: Bounds;
  actions: Bounds;
  form: Bounds;
  card: Bounds;
  transcriptScrollTop: number | undefined;
  priorSiblings: Bounds[];
};
declare global {
  interface Window {
    __snapshotOrderResourceFrames: Array<{
      at: number;
      frame: ResourceChangeEvent;
    }>;
    __snapshotOrderOpenStable: { previous: number[] | null; frames: number };
    __snapshotOrderLabelFrames: {
      phase: string;
      stableFrames: number;
      previous: GeometryFrame | null;
      frames: GeometryFrame[];
    };
  }
}

export function installSnapshotOrderProbe() {
  const NativeEventSource = window.EventSource;
  window.__snapshotOrderResourceFrames = [];
  window.EventSource = new Proxy(NativeEventSource, {
    construct(Target, args) {
      const stream: EventSource = Reflect.construct(Target, args);
      stream.addEventListener("resources", (event) => {
        window.__snapshotOrderResourceFrames.push({
          at: performance.timeOrigin + performance.now(),
          frame: JSON.parse(event.data),
        });
      });
      return stream;
    },
  });
}
