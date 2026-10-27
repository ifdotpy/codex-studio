// Owner of the subset of realtime vendor events used by the voice panel.
// Vendor reference: https://platform.openai.com/docs/api-reference/realtime-server-events
export type VoiceEvent =
  | { type: "ignored" }
  | { type: "error"; message: string }
  | { type: "session.started" | "delegation.created" | "turn.done" }
  | {
      type: "input_transcript.added" | "output_transcript.added";
      text: string;
    };

export function decodeVoiceEvent(value: unknown): VoiceEvent {
  if (!value || typeof value !== "object" || !("type" in value))
    return { type: "ignored" };
  switch (value.type) {
    case "session.started":
    case "delegation.created":
    case "turn.done":
      return { type: value.type };
    case "input_transcript.added":
    case "output_transcript.added":
      if (
        "item" in value &&
        value.item &&
        typeof value.item === "object" &&
        "text" in value.item &&
        typeof value.item.text === "string"
      )
        return { type: value.type, text: value.item.text };
      return { type: "ignored" };
    case "error":
      if (
        "error" in value &&
        value.error &&
        typeof value.error === "object" &&
        "message" in value.error &&
        typeof value.error.message === "string"
      )
        return { type: "error", message: value.error.message };
      return { type: "ignored" };
    default:
      return { type: "ignored" };
  }
}
