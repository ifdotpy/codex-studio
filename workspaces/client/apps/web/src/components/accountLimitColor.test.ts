import { expect, test } from "vitest";
import { remainingLimitColor } from "./accountLimitColor";

test.each([
  [100, "unread"],
  [50.01, "unread"],
  [50, "answer"],
  [15, "answer"],
  [14.99, "error"],
  [0, "error"],
  [null, "paused"],
] as const)("%s percent remaining uses the %s state", (remaining, state) => {
  expect(remainingLimitColor(remaining)).toBe(`var(--studio-state-${state})`);
});
