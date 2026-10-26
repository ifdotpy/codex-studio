declare global {
  interface Window {
    speechProgress:
      | Parameters<DesktopBridge["onTranscriptionProgress"]>[0]
      | undefined;
    rejectTranscription: (reason: Error) => void | undefined;
    attemptId: string;
    audioBytes: number;
    deferTranscription: boolean;
    finishTranscription:
      | ((value: Awaited<ReturnType<DesktopBridge["transcribeAudio"]>>) => void)
      | null;
    retryOK: boolean;
    emitAudio: () => void;
    captureStopped: number;
    failStop: boolean;
  }
}
window.attemptId = "";
window.audioBytes = 0;
window.deferTranscription = false;
window.finishTranscription = null;
window.retryOK = false;
window.emitAudio = () => {
  throw new Error("Audio fixture unavailable");
};
window.captureStopped = 0;
window.failStop = false;
const root = document.getElementById("root")!;
import type { DesktopBridge } from "../../src/desktop.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/studio-theme.css";
import { theme } from "../../src/theme.ts";
import { Dictation } from "../../src/components/Dictation.tsx";
// Base bridge fixture supplies only the five methods used by dictation.
const bridgeFixture = {
  requestMicrophone: async () => true,
  prepareTranscription: async () => "permit",
  onTranscriptionProgress: (callback) => {
    window.speechProgress = callback;
    return () => {};
  },
  cancelTranscription: async () => {
    window.rejectTranscription?.(
      Error("Transcription cancelled. Your recording is saved."),
    );
    return true;
  },
  transcribeAudio: async (value) => {
    window.attemptId = value.id;
    window.audioBytes = value.audio.byteLength;
    if (window.deferTranscription)
      return new Promise((resolve, reject) => {
        window.finishTranscription = resolve;
        window.rejectTranscription = reject;
      });
    if (!window.retryOK)
      throw Error("Recognition unavailable; recording preserved.");
    return {
      text: "Recovered spoken instruction",
      provider: "fixture",
      onDevice: true,
    };
  },
} satisfies Pick<
  DesktopBridge,
  | "requestMicrophone"
  | "prepareTranscription"
  | "onTranscriptionProgress"
  | "cancelTranscription"
  | "transcribeAudio"
>;
// The base document omits platform and unrelated desktop capabilities.
function partialBridge(value: unknown): DesktopBridge {
  return value as DesktopBridge;
}
window.codexDesktop = partialBridge(bridgeFixture);
function App() {
  const [chat, setChat] = useState("first"),
    [draft, setDraft] = useState("Original draft");
  return (
    <MantineProvider theme={theme} forceColorScheme="dark">
      <button onClick={() => setChat(chat === "first" ? "second" : "first")}>
        Switch chat
      </button>
      <textarea
        aria-label="Draft"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
      />
      <Dictation
        key={chat}
        chatId={chat}
        onInsert={(text) => setDraft((current) => current + "\n" + text)}
      />
    </MantineProvider>
  );
}
createRoot(root).render(<App />);
