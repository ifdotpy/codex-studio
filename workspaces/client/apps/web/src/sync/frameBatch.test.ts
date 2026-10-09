import { afterEach, expect, it, vi } from "vitest";
import { createFrameBatch } from "./frameBatch";

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("publishes once per frame and cancels its fallback timer", () => {
  vi.useFakeTimers();
  let frame: FrameRequestCallback | undefined;
  vi.stubGlobal("document", { hidden: false });
  vi.stubGlobal(
    "requestAnimationFrame",
    vi.fn((next) => ((frame = next), 1)),
  );
  const cancel = vi.fn();
  vi.stubGlobal("cancelAnimationFrame", cancel);
  const publish = vi.fn();
  const batch = createFrameBatch(publish);
  for (let index = 0; index < 100; index++) batch.schedule();
  expect(requestAnimationFrame).toHaveBeenCalledOnce();
  frame?.(16);
  expect(publish).toHaveBeenCalledOnce();
  vi.advanceTimersByTime(100);
  expect(publish).toHaveBeenCalledOnce();
  expect(cancel).toHaveBeenCalledWith(1);
  batch.schedule();
  batch.cancel();
  frame?.(32);
  vi.advanceTimersByTime(100);
  expect(publish).toHaveBeenCalledOnce();
});

it("uses a 50 millisecond timer for hidden tabs without an animation frame", () => {
  vi.useFakeTimers();
  vi.stubGlobal("document", { hidden: true });
  vi.stubGlobal("requestAnimationFrame", vi.fn());
  const publish = vi.fn();
  const batch = createFrameBatch(publish);
  batch.schedule();
  vi.advanceTimersByTime(49);
  expect(publish).not.toHaveBeenCalled();
  vi.advanceTimersByTime(1);
  expect(publish).toHaveBeenCalledOnce();
  expect(requestAnimationFrame).not.toHaveBeenCalled();
});

it("uses the timer when a visible tab does not receive its animation frame", () => {
  vi.useFakeTimers();
  vi.stubGlobal("document", { hidden: false });
  vi.stubGlobal(
    "requestAnimationFrame",
    vi.fn(() => 1),
  );
  vi.stubGlobal("cancelAnimationFrame", vi.fn());
  const publish = vi.fn();
  createFrameBatch(publish).schedule();
  vi.advanceTimersByTime(50);
  expect(publish).toHaveBeenCalledOnce();
});
