import { expect, it, vi } from "vitest";
import { bindAudioFrames } from "./audioFrames";
function caller() {
  const frame = {
    contentWindow: { postMessage: vi.fn() },
  } as unknown as HTMLIFrameElement;
  return { frame, origin: "studio-view://fixture.localhost", live: () => true };
}
it("binds speech permits, uploads, progress and cancellation to the initiating frame", async () => {
  let events: (value: unknown) => void = () => {};
  let finish!: (value: unknown) => void;
  const invoke = vi.fn(async (method: string) => {
    if (method === "desktop.prepareTranscription") return "permit";
    if (method === "speech.uploadBegin") return { transfer: "upload" };
    if (method === "speech.transcribe")
      return new Promise((resolve) => {
        finish = resolve;
      });
    return true;
  });
  const audio = bindAudioFrames(invoke, async (callback) => {
    events = callback;
    return vi.fn<() => void>();
  });
  const a = caller(),
    b = caller();
  await audio.request("desktop.prepareTranscription", {}, "prepare", a);
  await expect(
    audio.request(
      "speech.uploadBegin",
      { permit: "permit", size: 4 },
      "bad",
      b,
    ),
  ).rejects.toThrow("another view");
  await audio.request(
    "speech.uploadBegin",
    { permit: "permit", size: 4 },
    "begin",
    a,
  );
  await expect(
    audio.request("speech.uploadChunk", { transfer: "upload" }, "bad", b),
  ).rejects.toThrow("another view");
  await audio.request(
    "view.subscribe",
    { topic: "transcription", subscription: "progress" },
    "sub",
    a,
  );
  const subscription = (
    invoke.mock.calls.find(
      (call) => call[0] === "view.subscribe",
    ) as unknown as [string, { subscription: string }]
  )[1].subscription;
  const task = audio.request(
    "speech.transcribe",
    {
      transfer: "upload",
      permit: "permit",
      id: "attempt",
      locale: "en",
      recording: "private-record",
    },
    "run",
    a,
  );
  expect(invoke).toHaveBeenLastCalledWith(
    "speech.transcribe",
    { transfer: "upload", permit: "permit", id: "attempt", locale: "en" },
    "run",
  );
  events({ event: subscription, value: { id: "other", completed: 1 } });
  expect(a.frame.contentWindow!.postMessage).not.toHaveBeenCalled();
  events({ event: subscription, value: { id: "attempt", completed: 1 } });
  expect(a.frame.contentWindow!.postMessage).toHaveBeenCalledOnce();
  await expect(
    audio.request(
      "desktop.cancelTranscription",
      { value: "attempt" },
      "bad",
      b,
    ),
  ).rejects.toThrow("another view");
  await audio.request(
    "desktop.cancelTranscription",
    { value: "attempt" },
    "cancel",
    a,
  );
  finish({ text: "done" });
  await task;
  await expect(
    audio.request(
      "speech.transcribe",
      { transfer: "upload", permit: "permit", id: "retry" },
      "retry",
      a,
    ),
  ).rejects.toThrow();
  audio.dispose();
});
it("rewrites microphone identities and stops a detached owner without affecting other frames", async () => {
  let events: (value: unknown) => void = () => {};
  const invoke = vi.fn(
    async (_method: string, _params: unknown, _id?: string) => true,
  );
  const audio = bindAudioFrames(invoke, async (callback) => {
    events = callback;
    return vi.fn<() => void>();
  });
  const a = caller(),
    b = caller();
  await audio.request("desktop.requestMicrophone", {}, "mic", a);
  await expect(
    audio.request(
      "view.subscribe",
      { topic: "microphone", subscription: "spy", capture: "c" },
      "spy",
      b,
    ),
  ).rejects.toThrow();
  await audio.request(
    "view.subscribe",
    { topic: "microphone", subscription: "audio", capture: "c" },
    "sub",
    a,
  );
  const native = (
    invoke.mock.calls.find((call) => call[0] === "view.subscribe")![1] as {
      subscription: string;
    }
  ).subscription;
  expect(native).not.toBe("audio");
  await audio.request("media.captureStart", { capture: "c" }, "start", a);
  await expect(
    audio.request("media.captureStop", { capture: "c" }, "stop", b),
  ).rejects.toThrow();
  events({ event: native, value: { pcm: "AIAAAP9/" } });
  expect(a.frame.contentWindow!.postMessage).toHaveBeenCalledWith(
    {
      kind: "studio-native-audio-event",
      event: "audio",
      value: { pcm: "AIAAAP9/" },
    },
    a.origin,
  );
  expect(b.frame.contentWindow!.postMessage).not.toHaveBeenCalled();
  a.live = () => false;
  await audio.request("desktop.requestMicrophone", {}, "new-owner", b);
  expect(invoke).toHaveBeenCalledWith("media.captureStop", { capture: native });
  audio.dispose();
});

it("keeps the Rust voice peer and its events private to its owning server", async () => {
  let events: (value: unknown) => void = () => {};
  const invoke = vi.fn(
    async (_method: string, _params: unknown, _id?: string) => true,
  );
  const audio = bindAudioFrames(invoke, async (callback) => {
    events = callback;
    return () => {};
  });
  const a = caller(),
    b = caller();
  await audio.request("desktop.requestMicrophone", {}, "mic", a);
  await audio.request(
    "view.subscribe",
    { topic: "voice", subscription: "events", session: "logical" },
    "sub",
    a,
  );
  const params = invoke.mock.calls.find(
    (call) => call[0] === "view.subscribe",
  )![1] as { session: string; subscription: string };
  expect(params.session).toBe(params.subscription);
  await audio.request("voice.create", { session: "logical" }, "start", a);
  await expect(
    audio.request(
      "voice.answer",
      { session: "logical", sdp: "foreign" },
      "bad",
      b,
    ),
  ).rejects.toThrow();
  await expect(
    audio.request("voice.close", { session: "logical" }, "bad-close", b),
  ).rejects.toThrow();
  events({
    event: params.subscription,
    value: { type: "state", state: "connected" },
  });
  expect(a.frame.contentWindow!.postMessage).toHaveBeenCalledOnce();
  expect(b.frame.contentWindow!.postMessage).not.toHaveBeenCalled();
  audio.dispose();
  expect(invoke).toHaveBeenCalledWith("voice.close", {
    session: params.session,
  });
});
it("isolates direct server owners and releases capture when an owner closes", async () => {
  let events: (value: unknown) => void = () => {};
  const invoke = vi.fn(
    async (_method: string, _params: unknown, _id?: string) => true,
  );
  const audio = bindAudioFrames(invoke, async (callback) => {
    events = callback;
    return () => {};
  });
  const a = audio.createClient();
  const b = audio.createClient();
  const receivedA = vi.fn();
  const receivedB = vi.fn();
  await a.listen(receivedA);
  await b.listen(receivedB);
  await a.request("desktop.requestMicrophone", {}, "gesture-a");
  await expect(b.request("desktop.requestMicrophone", {})).rejects.toThrow(
    "Another server",
  );
  await a.request("view.subscribe", {
    topic: "microphone",
    subscription: "same",
    capture: "same",
  });
  const nativeA = (
    invoke.mock.calls.find((call) => call[0] === "view.subscribe")![1] as {
      subscription: string;
    }
  ).subscription;
  await a.request("media.captureStart", { capture: "same" });
  events({ event: nativeA, value: { pcm: "AAAA" } });
  expect(receivedA).toHaveBeenCalledWith({
    event: "same",
    value: { pcm: "AAAA" },
  });
  expect(receivedB).not.toHaveBeenCalled();
  await expect(
    b.request("media.captureStop", { capture: "same" }),
  ).rejects.toThrow();
  a.close();
  expect(invoke).toHaveBeenCalledWith("media.captureStop", {
    capture: nativeA,
  });
  expect(invoke).toHaveBeenCalledWith("view.unsubscribe", {
    subscription: nativeA,
  });
  await expect(a.request("desktop.requestMicrophone", {})).rejects.toThrow(
    "closed",
  );
  await b.request("desktop.requestMicrophone", {}, "gesture-b");
  await b.request("view.subscribe", {
    topic: "microphone",
    subscription: "same",
    capture: "same",
  });
  const nativeB = (
    invoke.mock.calls
      .filter((call) => call[0] === "view.subscribe")
      .at(-1)![1] as { subscription: string }
  ).subscription;
  expect(nativeB).not.toBe(nativeA);
  events({ event: nativeA, value: { pcm: "AAAA" } });
  expect(receivedA).toHaveBeenCalledTimes(1);
  expect(receivedB).not.toHaveBeenCalled();
  events({ event: nativeB, value: { pcm: "BBBB" } });
  expect(receivedB).toHaveBeenCalledOnce();
  await b.request(
    "view.unsubscribe",
    { subscription: "same" },
    "unsubscribe-b",
  );
  expect(invoke).toHaveBeenLastCalledWith(
    "view.unsubscribe",
    { subscription: nativeB },
    "unsubscribe-b",
  );
  b.close();
  audio.dispose();
});
