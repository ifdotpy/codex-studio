import { afterEach, expect, test, vi } from "vitest";
import { streamTranscriptFixture } from "../tests/fixtures/streaming-transcript.mjs";

afterEach(() => vi.useRealTimers());

test("the scroll fixture waits for a delayed render before publishing another delta", async () => {
  vi.useFakeTimers();
  const admission = Promise.withResolvers();
  let running = true;
  const publish = vi.fn(async () => "first delta");
  const rendered = vi.fn(() => admission.promise);
  const stream = streamTranscriptFixture({
    running: () => running,
    publish,
    rendered,
  });
  try {
    await vi.advanceTimersByTimeAsync(500);
    expect(publish).toHaveBeenCalledTimes(1);
    expect(rendered).toHaveBeenCalledWith("first delta");
  } finally {
    running = false;
    admission.resolve();
    await vi.runAllTimersAsync();
    await stream;
  }
});

test("the scroll fixture continues after rendering and stops without another publication", async () => {
  vi.useFakeTimers();
  let running = true;
  let sequence = 0;
  const publish = vi.fn(async () => ++sequence);
  const rendered = vi.fn(async () => {
    if (sequence === 2) running = false;
  });
  const stream = streamTranscriptFixture({
    running: () => running,
    publish,
    rendered,
  });
  try {
    await vi.advanceTimersByTimeAsync(100);
    expect(publish).toHaveBeenCalledTimes(2);
    expect(rendered.mock.calls).toEqual([[1], [2]]);
  } finally {
    running = false;
    await vi.runAllTimersAsync();
    await stream;
  }
});

test("the scroll fixture reports a render failure and stops publication", async () => {
  const error = new Error("The streamed reply did not render");
  const publish = vi.fn(async () => "unrendered delta");
  await expect(
    streamTranscriptFixture({
      running: () => true,
      publish,
      rendered: async () => {
        throw error;
      },
    }),
  ).rejects.toBe(error);
  expect(publish).toHaveBeenCalledTimes(1);
});
