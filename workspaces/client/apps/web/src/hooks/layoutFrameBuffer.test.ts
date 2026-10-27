import { expect, it } from "vitest";
import {
  createLayoutFrameBuffer,
  type LayoutFrameSample,
} from "./layoutFrameBuffer";
const sample = (time: number, offsetTop = 0): LayoutFrameSample => ({
  time,
  interceptedGestures: 0,
  phase: "focus",
  innerHeight: offsetTop ? 323 : 669,
  height: offsetTop ? 323 : 669,
  offsetTop,
  pageTop: offsetTop,
  scrollY: offsetTop,
  rootTop: 0,
  sidebar: [0, 0, 227, 597],
  composer: [232, 323],
});
it("retains the silent frames immediately before a delayed native pan, then the close", () => {
  const buffer = createLayoutFrameBuffer(3);
  for (let t = 0; t < 20; t++) buffer.push(sample(t));
  buffer.push(sample(20, 3));
  buffer.push(sample(21, 13));
  for (let t = 22; t < 40; t++) buffer.push(sample(t, 346));
  buffer.push({ ...sample(40), phase: "blur" });
  buffer.push({ ...sample(41), phase: "blur" });
  expect(buffer.snapshot().map((row) => row.time)).toEqual([
    17, 18, 19, 20, 21, 22, 39, 40, 41,
  ]);
});
it("starts a new baseline for the next focus cycle", () => {
  const buffer = createLayoutFrameBuffer(3);
  buffer.push(sample(0, 346));
  buffer.reset();
  for (let t = 10; t < 20; t++) buffer.push(sample(t));
  expect(buffer.snapshot().map((row) => row.time)).toEqual([17, 18, 19]);
});
