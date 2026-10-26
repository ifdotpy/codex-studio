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

/** Owner of primary touch input for mobile layout and keyboard gestures. */
export function primaryTouchPointerMedia() {
  return window.matchMedia("(pointer: coarse)");
}
