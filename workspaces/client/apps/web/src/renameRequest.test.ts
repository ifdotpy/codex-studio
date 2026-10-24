import { afterEach, expect, it, vi } from "vitest";
import { submitRename, watchRenameResult } from "./renameRequest";
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

it("delivers rename failure from the request-scoped stream without HTTP", () => {
  let deliver!: (
    version?: import("./sync/resourceEvents").ResourceVersion,
  ) => void;
  const release = vi.fn();
  const events = {
    watchResourceChanges: vi.fn((_resource, callback) => {
      deliver = callback;
      return release;
    }),
  };
  const accept = vi.fn();
  const stop = watchRenameResult(events, "agent", "request", accept);
  expect(events.watchResourceChanges).toHaveBeenCalledTimes(1);
  const resource = {
    kind: "rename-progress",
    agentId: "agent",
    requestId: "request",
  };
  const value = {
    id: "agent",
    request_id: "request",
    status: "failed",
    error: "No title",
  };
  deliver({
    epoch: "owner",
    revision: 1,
    progress: { resource, value },
  } as import("./sync/resourceEvents").ResourceVersion);
  expect(accept).toHaveBeenCalledWith(value);
  stop();
  expect(release).toHaveBeenCalledTimes(1);
});
