import type { NativeServerContext } from "./client";

export type NativeVoiceState = {
  active: boolean;
  sessionId?: string;
  status: string;
  partial: string;
  muted: boolean;
  error?: string;
};

/** The exact native context owns signaling, capture, and session deadlines. */
export function createNativeVoiceSession(
  context: NativeServerContext,
  agent: string,
) {
  return {
    permit: () => context.request("desktop.requestMicrophone", {}),
    start: (requestId: string) =>
      context.request<NativeVoiceState>(
        "voice.sessionStart",
        {
          agent,
          muted: false,
        },
        requestId,
      ),
    read: (requestId: string) =>
      context.request<NativeVoiceState>("voice.sessionState", {
        agent,
        requestId,
      }),
    end: (sessionId: string) =>
      context.request<NativeVoiceState>("voice.sessionEnd", {
        agent,
        sessionId,
      }),
    mute: (sessionId: string, muted: boolean) =>
      context.request<NativeVoiceState>("voice.sessionMute", {
        agent,
        sessionId,
        muted,
      }),
  };
}
