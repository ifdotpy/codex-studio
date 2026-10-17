import { beforeEach, afterEach, expect, it, vi } from "vitest";
vi.mock("./client", () => ({
  hasNativeClient: () => true,
  nativeHost: vi.fn(),
  nativeRequest: vi.fn(),
}));
vi.mock("./frame", () => ({ isNativeFrame: () => false }));
import { nativeHost, nativeRequest } from "./client";
import { notifyDesktop, onDesktopNavigate } from "./notifications";
beforeEach(() => vi.stubGlobal("window", {}));
afterEach(() => {
  vi.resetAllMocks();
  vi.unstubAllGlobals();
});
it("preserves the server and chat destination through native notifications", async () => {
  const notification = {
    title: "Remote",
    body: "Reply",
    target: {
      serverId: "other",
      agentId: "chat",
      section: "messages" as const,
    },
  };
  vi.mocked(nativeRequest).mockResolvedValue(true);
  await expect(notifyDesktop(notification)).resolves.toBe(true);
  expect(nativeRequest).toHaveBeenCalledWith("desktop.notify", notification);
});
it("disposes a late native subscription without delivering stale navigation", async () => {
  let finish!: (stop: () => void) => void;
  let event!: (value: { payload: unknown }) => void;
  const listen = vi.fn((_name, callback) => {
    event = callback;
    return new Promise<() => void>((resolve) => {
      finish = resolve;
    });
  });
  vi.mocked(nativeHost).mockReturnValue({
    core: { invoke: vi.fn() },
    event: { listen },
  });
  const callback = vi.fn();
  const dispose = onDesktopNavigate(callback);
  dispose();
  event({ payload: { serverId: "other", agentId: "chat" } });
  const stop = vi.fn();
  finish(stop);
  await Promise.resolve();
  expect(stop).toHaveBeenCalledOnce();
  expect(callback).not.toHaveBeenCalled();
});
