import { expect, it, vi } from "vitest";
import type { NativeServerContext } from "./client";
import { createNativeVoiceSession } from "./voiceSession";

it("keeps lost Start as one native command and recovers by a read only", async () => {
  const request = vi
    .fn()
    .mockRejectedValueOnce(new Error("reply lost"))
    .mockResolvedValueOnce({ active: true, sessionId: "original-session" });
  const voice = createNativeVoiceSession(
    { request } as unknown as NativeServerContext,
    "original-chat",
  );
  await expect(voice.start("original-start")).rejects.toThrow("reply lost");
  await expect(voice.read("original-start")).resolves.toMatchObject({
    sessionId: "original-session",
  });
  expect(request.mock.calls).toEqual([
    [
      "voice.sessionStart",
      { agent: "original-chat", muted: false },
      "original-start",
    ],
    [
      "voice.sessionState",
      { agent: "original-chat", requestId: "original-start" },
    ],
  ]);
});
it("ends and mutes only the original session in its captured native context", async () => {
  const request = vi.fn().mockResolvedValue({ active: false });
  const other = vi.fn();
  const voice = createNativeVoiceSession(
    { request } as unknown as NativeServerContext,
    "chat-a",
  );
  createNativeVoiceSession(
    { request: other } as unknown as NativeServerContext,
    "chat-b",
  );
  await voice.mute("session-a", true);
  await voice.end("session-a");
  expect(request.mock.calls).toEqual([
    [
      "voice.sessionMute",
      { agent: "chat-a", sessionId: "session-a", muted: true },
    ],
    ["voice.sessionEnd", { agent: "chat-a", sessionId: "session-a" }],
  ]);
  expect(other).not.toHaveBeenCalled();
});
