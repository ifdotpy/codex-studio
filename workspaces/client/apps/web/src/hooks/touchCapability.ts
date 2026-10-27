/** Owner of touch capability for viewport and safe-strip controls. */
export function touchPointerMedia() {
  return window.matchMedia("(any-pointer: coarse)");
}

export function hasTouchCapability(
  anyCoarsePointer: boolean,
  maxTouchPoints: number,
) {
  return anyCoarsePointer || maxTouchPoints > 0;
}
