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
) {
  const subscription = crypto.randomUUID();
  let closed = false;
  let listening: (() => void) | undefined;
  const unsubscribe = () =>
    nativeRequest(isNativeFrame() ? "audio.unsubscribe" : "view.unsubscribe", {
      subscription,
    }).catch(() => {});
  const closeNative = () =>
    nativeRequest("voice.close", { session }).catch(() => {});
  return {
    async offer() {
      try {
        listening = await listenNativeEvents((event: any) => {
          if (!closed && event?.event === subscription) onEvent(event.value);
        });
        if (closed) throw new Error("Voice ended.");
        await nativeRequest("view.subscribe", {
          topic: "voice",
          subscription,
          session,
        });
        if (closed) throw new Error("Voice ended.");
        const result = await nativeRequest<{ sdp: string }>("voice.create", {
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
      return nativeRequest("voice.answer", { session, sdp });
    },
    mute(muted: boolean) {
      return nativeRequest("voice.mute", { session, muted });
    },
    close() {
      closed = true;
      listening?.();
      // A late offer also closes itself, covering teardown during creation.
      return closeNative().finally(unsubscribe);
    },
  };
}
