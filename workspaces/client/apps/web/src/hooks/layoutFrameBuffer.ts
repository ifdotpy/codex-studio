export type LayoutFrameSample = {
  time: number;
  interceptedGestures: number;
  phase: string;
  innerHeight: number;
  height: number | undefined;
  offsetTop: number | undefined;
  pageTop: number | undefined;
  scrollY: number;
  rootTop: number;
  sidebar: number[] | null;
  composer: number[] | null;
};

/** Keep the raw frames before a delayed pan, after its first change, and at close. */
export function createLayoutFrameBuffer(windowFrames: number) {
  let before: LayoutFrameSample[] = [],
    after: LayoutFrameSample[] = [],
    latest: LayoutFrameSample[] = [];
  let baseline: (number | undefined)[] | null = null;
  let triggered = false;
  const signature = (sample: LayoutFrameSample) => [
    sample.innerHeight,
    sample.height,
    sample.offsetTop,
    sample.pageTop,
    sample.scrollY,
    sample.rootTop,
  ];
  return {
    reset() {
      before = [];
      after = [];
      latest = [];
      baseline = null;
      triggered = false;
    },
    push(sample: LayoutFrameSample) {
      const geometry = signature(sample);
      if (baseline === null) baseline = geometry;
      const initial = baseline;
      if (
        !triggered &&
        geometry.some((value, index) => value !== initial[index])
      )
        triggered = true;
      if (!triggered) {
        before.push(sample);
        if (before.length > windowFrames) before.shift();
      } else if (after.length < windowFrames) after.push(sample);
      else {
        latest.push(sample);
        if (latest.length > windowFrames) latest.shift();
      }
    },
    snapshot() {
      return [...before, ...after, ...latest];
    },
  };
}
