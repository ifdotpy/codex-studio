import { expect, it } from "vitest";
import { canUnloadFrame, FRAME_IDLE_MS } from "./idleFrames";
it("unloads only a ready hidden idle frame after five minutes", () => {
  expect(canUnloadFrame(false, true, false, 10, 10 + FRAME_IDLE_MS)).toBe(true);
  expect(canUnloadFrame(false, true, false, 10, 10 + FRAME_IDLE_MS - 1)).toBe(
    false,
  );
  expect(canUnloadFrame(true, true, false, 10, 10 + FRAME_IDLE_MS)).toBe(false);
  expect(canUnloadFrame(false, false, false, 10, 10 + FRAME_IDLE_MS)).toBe(
    false,
  );
  expect(canUnloadFrame(false, true, true, 10, 10 + FRAME_IDLE_MS)).toBe(false);
});
