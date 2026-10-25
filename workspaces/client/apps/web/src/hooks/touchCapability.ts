/** Owner of touch capability for viewport and safe-strip controls. */
export const touchPointerQuery = "(any-pointer: coarse)";

export function hasTouchCapability(
  anyCoarsePointer: boolean,
  maxTouchPoints: number,
) {
  return anyCoarsePointer || maxTouchPoints > 0;
}
