import { listenNativeEvents, nativeRequest } from "./client";
import { isNativeFrame } from "./frame";
export type NativeVoiceEvent = {
  type: string;
  state?: string;
  data?: string;
  message?: string;
};
export function nativeVoice(
  session: string,
  onEvent: (event: NativeVoiceEvent) => void,
  request: typeof nativeRequest = nativeRequest,
  listen: typeof listenNativeEvents = listenNativeEvents,
) {
  const subscription = crypto.randomUUID();
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
        listening = await listen((event) => {
          if (
            closed ||
            !event ||
            typeof event !== "object" ||
            !("event" in event) ||
            event.event !== subscription ||
            !("value" in event)
          )
            return;
          const value = event.value;
          if (
            !value ||
            typeof value !== "object" ||
            !("type" in value) ||
            typeof value.type !== "string"
          )
            return;
          if (
            ["state", "data", "message"].some(
              (key) =>
                key in value && typeof Reflect.get(value, key) !== "string",
            )
          )
            return;
          onEvent(value as NativeVoiceEvent);
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
