import { randomUUID } from "../uuid";
import type { NativeVoiceEvent } from "../generated/rustOwned";
import { listenNativeEvents, nativeRequest } from "./client";
import { isNativeFrame } from "./frame";
function decodeNativeVoiceEvent(value: unknown): NativeVoiceEvent | undefined {
  if (!value || typeof value !== "object" || !("type" in value)) return;
  switch (value.type) {
    case "message":
      if ("data" in value && typeof value.data === "string")
        return { type: value.type, data: value.data };
      break;
    case "state":
    case "channel":
    case "microphone":
      if ("state" in value && typeof value.state === "string")
        return { type: value.type, state: value.state };
      break;
    case "error":
      if ("message" in value && typeof value.message === "string")
        return { type: value.type, message: value.message };
  }
  return undefined;
}
export function nativeVoice(
  session: string,
  onEvent: (event: NativeVoiceEvent) => void,
  request: typeof nativeRequest = nativeRequest,
  listen: typeof listenNativeEvents = listenNativeEvents,
) {
  const subscription = randomUUID();
  let closed = false;
  let listening: (() => void) | undefined;
  const unsubscribe = () =>
    request(isNativeFrame() ? "audio.unsubscribe" : "view.unsubscribe", {
      subscription,
    }).catch(() => {});
  const closeNative = () => request("voice.close", { session }).catch(() => {});
  return {
    async offer() {
      try {
        listening = await listen((input) => {
          if (
            closed ||
            !input ||
            typeof input !== "object" ||
            !("event" in input) ||
            input.event !== subscription ||
            !("value" in input)
          )
            return;
          const event = decodeNativeVoiceEvent(input.value);
          if (event) onEvent(event);
        });
        if (closed) throw new Error("Voice ended.");
        await request("view.subscribe", {
          topic: "voice",
          subscription,
          session,
        });
        if (closed) throw new Error("Voice ended.");
        const result = await request<{ sdp: string }>("voice.create", {
          session,
        });
        if (closed) throw new Error("Voice ended.");
        return result.sdp;
      } catch (error) {
        listening?.();
        await closeNative();
        await unsubscribe();
        throw error;
      }
    },
    answer(sdp: string) {
      return request("voice.answer", { session, sdp });
    },
    mute(muted: boolean) {
      return request("voice.mute", { session, muted });
    },
    close() {
      closed = true;
      listening?.();
      // A late offer also closes itself, covering teardown during creation.
      return closeNative().finally(unsubscribe);
    },
  };
}
