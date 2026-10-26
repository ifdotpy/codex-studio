import { afterEach, expect, it, vi } from "vitest";
vi.mock("./client", () => ({
  nativeRequest: vi.fn(),
  listenNativeEvents: vi.fn(),
  hasNativeClient: () => true,
}));
vi.mock("./frame", () => ({ isNativeFrame: () => false }));
import { nativeRequest, listenNativeEvents } from "./client";
import { captureNativeAudio, dictationBridge } from "./dictation";
afterEach(() => {
  vi.resetAllMocks();
  vi.unstubAllGlobals();
});
it("uploads exact bounded PCM WAV bytes and never repeats a lost recognition response", async () => {
  vi.stubGlobal("window", {});
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "speech.uploadBegin") return { transfer: "speech" };
    if (method === "speech.transcribe") throw new Error("lost reply");
    return null;
  });
  const bytes = Uint8Array.from({ length: 300001 }, (_, i) => i % 256);
  await expect(
    dictationBridge()!.transcribeAudio({
      id: "one",
      permit: "p",
      audio: bytes.buffer,
      locale: "en",
    }),
  ).rejects.toThrow("lost reply");
  const calls = vi.mocked(nativeRequest).mock.calls;
  const chunks = calls
    .filter(([method]) => method === "speech.uploadChunk")
    .map(([, value]) => value as { offset: number; base64: string });
  expect(chunks.map((c) => c.offset)).toEqual([0, 262144]);
  expect(
    Buffer.concat(chunks.map((c) => Buffer.from(c.base64, "base64"))).equals(
      Buffer.from(bytes),
    ),
  ).toBe(true);
  expect(
    calls.filter(([method]) => method === "speech.transcribe"),
  ).toHaveLength(1);
  expect(calls.at(-1)).toEqual(["speech.uploadCancel", { transfer: "speech" }]);
});
it("cancellation during upload preserves the source and does not start recognition", async () => {
  vi.stubGlobal("window", {});
  const speech = dictationBridge()!;
  vi.mocked(nativeRequest).mockImplementation(async (method) => {
    if (method === "speech.uploadBegin") return { transfer: "speech" };
    if (method === "speech.uploadChunk")
      await speech.cancelTranscription("cancel");
    return null;
  });
  await expect(
    speech.transcribeAudio({
      id: "cancel",
      permit: "p",
      audio: new ArrayBuffer(300000),
      locale: "en",
    }),
  ).rejects.toThrow("cancelled");
  expect(
    vi
      .mocked(nativeRequest)
      .mock.calls.some(([m]) => m === "speech.transcribe"),
  ).toBe(false);
});
it("subscribes before capture and retains the final signed PCM chunk before cleanup", async () => {
  let events!: (value: unknown) => void;
  let subscription = "";
  const stopListening = vi.fn();
  vi.mocked(listenNativeEvents).mockImplementation(async (callback) => {
    events = callback;
    return stopListening;
  });
  vi.mocked(nativeRequest).mockImplementation(async (method, params) => {
    if (method === "view.subscribe") {
      if (
        !params ||
        typeof params !== "object" ||
        !("subscription" in params) ||
        typeof params.subscription !== "string"
      )
        throw new Error("Missing subscription.");
      subscription = params.subscription;
    }
    if (method === "media.captureStart") return { sampleRate: 16000 };
    if (method === "media.captureStop") {
      events({ event: subscription, value: { pcm: "AIAAAP9/" } });
      events({ event: subscription, value: { stopped: true } });
    }
    return null;
  });
  const chunks: number[][] = [];
  const capture = await captureNativeAudio((pcm) => chunks.push([...pcm]));
  expect(
    vi
      .mocked(nativeRequest)
      .mock.calls.slice(0, 2)
      .map(([m]) => m),
  ).toEqual(["view.subscribe", "media.captureStart"]);
  await capture.stop();
  await capture.stop();
  expect(chunks).toEqual([[-32768, 0, 32767]]);
  expect(stopListening).toHaveBeenCalledOnce();
  expect(
    vi
      .mocked(nativeRequest)
      .mock.calls.filter(([m]) => m === "media.captureStop"),
  ).toHaveLength(1);
});
