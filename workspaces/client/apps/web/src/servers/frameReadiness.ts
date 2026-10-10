export type FrameLifecycle = { known: boolean; ready: boolean };

export function setServerFrame<T>(
  id: string,
  frame: T,
  frames: Map<string, T>,
  readyFrames: Set<string>,
  lifecycle: Map<string, FrameLifecycle>,
) {
  if (frames.get(id) !== frame) {
    readyFrames.delete(id);
    const state = lifecycle.get(id);
    if (state) {
      state.known = false;
      state.ready = false;
    }
  }
  frames.set(id, frame);
}
