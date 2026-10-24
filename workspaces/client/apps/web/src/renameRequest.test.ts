import { afterEach, expect, it, vi } from "vitest";
import { submitRename } from "./renameRequest";
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});
it("a pending rename sends one POST and waits for the entity stream", async () => {
  vi.useFakeTimers();
  vi.stubGlobal("window", globalThis);
  const post = vi.fn(async () => ({ id: "agent", status: "pending" as const }));
  const recover = vi.fn(async () => {});
  await submitRename({ id: "agent", request_id: "request" }, post, recover);
  await vi.advanceTimersByTimeAsync(90_000);
  expect(post).toHaveBeenCalledTimes(1);
  expect(recover).not.toHaveBeenCalled();
});
it("a lost response performs one recovery read and preserves the uncertain result", async () => {
  const lost = new TypeError("response lost");
  const post = vi.fn(async () => {
    throw lost;
  });
  const recover = vi.fn(async () => {});
  await expect(
    submitRename({ id: "agent", request_id: "request" }, post, recover),
  ).rejects.toBe(lost);
  expect(post).toHaveBeenCalledTimes(1);
  expect(recover).toHaveBeenCalledTimes(1);
});
