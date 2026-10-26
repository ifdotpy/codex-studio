import { assert } from "vitest";
import { afterEach, expect, it, vi } from "vitest";
vi.mock("./client", () => ({
  nativeRequest: vi.fn(),
  listenNativeEvents: vi.fn(),
  hasNativeClient: () => true,
}));
vi.mock("./frame", () => ({ isNativeFrame: () => false }));
import { nativeRequest, listenNativeEvents } from "./client";
import { createServerAudio } from "./serverAudio";

afterEach(() => {
  vi.resetAllMocks();
  vi.unstubAllGlobals();
});
it("routes dictation and voice through the same connection owner and closes only its resources", async () => {
  vi.stubGlobal("window", {});
  let events: (value: unknown) => void = () => {};
  const stopEvents = vi.fn();
  vi.mocked(listenNativeEvents).mockImplementation(async (callback) => {
    events = callback;
    return stopEvents;
  });
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "voice.create") return { sdp: "offer" };
    return true;
  });
  const a = createServerAudio(),
    b = createServerAudio();
  try {
    await a.dictationBridge()!.requestMicrophone();
    await expect(b.dictationBridge()!.requestMicrophone()).rejects.toThrow(
      "Another server",
    );
    const received = vi.fn();
    const voiceA = a.nativeVoice("same-session", received);
    expect(await voiceA.offer()).toBe("offer");
    const subscription = vi
      .mocked(nativeRequest)
      .mock.calls.find(([method]) => method === "view.subscribe")![1] as {
      subscription: string;
    };
    events({ event: subscription.subscription, value: { type: "ready" } });
    expect(received).toHaveBeenCalledWith({ type: "ready" });
    await expect(
      b.nativeVoice("same-session", vi.fn()).answer("answer"),
    ).rejects.toThrow("Unknown voice");
    a.close();
    events({ event: subscription.subscription, value: { type: "late" } });
    expect(received).toHaveBeenCalledTimes(1);
    expect(nativeRequest).toHaveBeenCalledWith("voice.close", {
      session: subscription.subscription,
    });
    await b.dictationBridge()!.requestMicrophone();
    const voiceB = b.nativeVoice("same-session", vi.fn());
    expect(await voiceB.offer()).toBe("offer");
    await voiceB.close();
  } finally {
    a.close();
    b.close();
  }
  await Promise.resolve();
  expect(stopEvents).toHaveBeenCalledOnce();
});
it("keeps cancelled attempt IDs local and does not replay an uncertain transcription", async () => {
  vi.stubGlobal("window", {});
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "desktop.prepareTranscription") return "permit";
    if (method === "speech.uploadBegin") return { transfer: "upload" };
    if (method === "speech.transcribe") throw new Error("lost reply");
    return true;
  });
  const a = createServerAudio(),
    b = createServerAudio();
  try {
    await a.dictationBridge()!.cancelTranscription("same-attempt");
    const permit = await b.dictationBridge()!.prepareTranscription();
    await expect(
      b.dictationBridge()!.transcribeAudio({
        id: "same-attempt",
        permit,
        audio: new ArrayBuffer(4),
        locale: "en",
      }),
    ).rejects.toThrow("lost reply");
    const calls = vi
      .mocked(nativeRequest)
      .mock.calls.filter(([method]) => method === "speech.transcribe");
    expect(calls).toHaveLength(1);
    assert(calls[0] !== undefined);
    expect(calls[0][1]).toEqual({
      id: "same-attempt",
      transfer: "upload",
      permit,
      locale: "en",
    });
  } finally {
    a.close();
    b.close();
  }
});
