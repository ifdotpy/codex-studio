import { expect, it } from "vitest";
import { decodeVoiceEvent } from "./voiceEvent";

it("ignores null, malformed transcript fields and unknown vendor events", () => {
  expect(decodeVoiceEvent(null)).toEqual({ type: "ignored" });
  expect(
    decodeVoiceEvent({ type: "input_transcript.added", item: { text: {} } }),
  ).toEqual({ type: "ignored" });
  expect(decodeVoiceEvent({ type: "vendor.future", extension: true })).toEqual({
    type: "ignored",
  });
  expect(
    decodeVoiceEvent({
      type: "input_transcript.added",
      item: { text: "hello" },
      extension: true,
    }),
  ).toEqual({ type: "input_transcript.added", text: "hello" });
});
