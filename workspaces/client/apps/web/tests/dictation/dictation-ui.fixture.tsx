declare global {
  interface Window {
    dictationFixture: {
      speechProgress?: Parameters<DesktopBridge["onTranscriptionProgress"]>[0];
      rejectTranscription?: (reason: Error) => void;
      attemptId: string;
      audioBytes: number;
      deferTranscription: boolean;
      finishTranscription:
        | ((
            value: Awaited<ReturnType<DesktopBridge["transcribeAudio"]>>,
          ) => void)
        | null;
      retryOK: boolean;
      emitAudio: () => void;
      captureStopped: number;
      failStop: boolean;
    };
  }
}
window.dictationFixture = {
  attemptId: "",
  audioBytes: 0,
  deferTranscription: false,
  finishTranscription: null,
  retryOK: false,
  emitAudio: () => {
    throw new Error("Audio fixture unavailable");
  },
  captureStopped: 0,
  failStop: false,
};
const root = document.getElementById("root");
if (!root) throw new Error("Missing dictation fixture root");
import { desktopBridgeFixture } from "../desktop-bridge-fixture.ts";
import type { DesktopBridge } from "../../src/desktop.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/studio-theme.css";
import { theme } from "../../src/theme.ts";
import { Dictation } from "../../src/components/Dictation.tsx";
window.codexDesktop = desktopBridgeFixture({
  requestMicrophone: async () => true,
  prepareTranscription: async () => "permit",
  onTranscriptionProgress: (callback) => {
    window.dictationFixture.speechProgress = callback;
    return () => {};
  },
  cancelTranscription: async () => {
    window.dictationFixture.rejectTranscription?.(
      Error("Transcription cancelled. Your recording is saved."),
    );
    return true;
  },
  transcribeAudio: async (value) => {
    window.dictationFixture.attemptId = value.id;
    window.dictationFixture.audioBytes = value.audio.byteLength;
    if (window.dictationFixture.deferTranscription)
      return new Promise((resolve, reject) => {
        window.dictationFixture.finishTranscription = resolve;
        window.dictationFixture.rejectTranscription = reject;
      });
    if (!window.dictationFixture.retryOK)
      throw Error("Recognition unavailable; recording preserved.");
    return {
      text: "Recovered spoken instruction",
      provider: "fixture",
      onDevice: true,
    };
  },
});
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
