/** Remaining provider capacity, in percent. These are presentation thresholds. */
const comfortableRemainingPercent = 50;
const lowRemainingPercent = 15;

export function remainingLimitColor(remaining: number | null): string {
  const state =
    remaining === null
      ? "paused"
      : remaining > comfortableRemainingPercent
        ? "unread"
        : remaining >= lowRemainingPercent
          ? "answer"
          : "error";
  return `var(--studio-state-${state})`;
}
