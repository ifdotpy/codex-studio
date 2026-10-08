export const FRAME_IDLE_MS = 5 * 60 * 1000;
export function canUnloadFrame(
  selected: boolean,
  ready: boolean,
  busy: boolean,
  idleSince: number,
  now: number,
) {
  return !selected && ready && !busy && now - idleSince >= FRAME_IDLE_MS;
}
