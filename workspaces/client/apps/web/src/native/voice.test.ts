/// <reference lib="es2024.promise" />
import { afterEach, expect, it, vi } from "vitest";
vi.mock("./client", () => ({
  nativeRequest: vi.fn(),
  listenNativeEvents: vi.fn(),
}));
vi.mock("./frame", () => ({ isNativeFrame: () => false }));
import { nativeRequest, listenNativeEvents } from "./client";
import { nativeVoice } from "./voice";
afterEach(() => vi.resetAllMocks());
it("subscribes before creating a Rust peer, then applies answer, mute and close", async () => {
  let emit!: (value: unknown) => void;
  let subscription = "";
  const stop = vi.fn<() => void>();
  vi.mocked(listenNativeEvents).mockImplementation(async (callback) => {
    emit = callback;
    return stop;
  });
  vi.mocked(nativeRequest).mockImplementation(async (method, params: any) => {
    if (method === "view.subscribe") subscription = params.subscription;
    return method === "voice.create" ? { sdp: "offer" } : true;
  });
  const receive = vi.fn();
  const peer = nativeVoice("session", receive);
  expect(await peer.offer()).toBe("offer");
  expect(
    vi
      .mocked(nativeRequest)
      .mock.calls.slice(0, 2)
      .map(([m]) => m),
  ).toEqual(["view.subscribe", "voice.create"]);
  emit({ event: "other", value: { type: "error" } });
  expect(receive).not.toHaveBeenCalled();
  emit({ event: subscription, value: { type: "message", data: "hello" } });
  expect(receive).toHaveBeenCalledWith({ type: "message", data: "hello" });
  await peer.answer("answer");
  await peer.mute(true);
  await peer.close();
  expect(nativeRequest).toHaveBeenCalledWith("voice.answer", {
    session: "session",
    sdp: "answer",
  });
  expect(nativeRequest).toHaveBeenCalledWith("voice.mute", {
    session: "session",
    muted: true,
  });
  expect(stop).toHaveBeenCalledOnce();
});
it("closes a late native offer after the chat was left", async () => {
  const entered = Promise.withResolvers<void>();
  let finish!: (value: unknown) => void;
  vi.mocked(listenNativeEvents).mockResolvedValue(() => {});
  vi.mocked(nativeRequest).mockImplementation(async (method) =>
    method === "voice.create"
      ? new Promise((resolve) => {
          finish = resolve;
          entered.resolve();
        })
      : true,
  );
  const peer = nativeVoice("late", vi.fn());
  const offer = peer.offer();
  await entered.promise;
  await peer.close();
  finish({ sdp: "late-offer" });
  await expect(offer).rejects.toThrow("Voice ended");
  expect(
    vi.mocked(nativeRequest).mock.calls.filter(([m]) => m === "voice.close"),
  ).toHaveLength(2);
});
