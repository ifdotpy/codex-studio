import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { Dictation } from "../../src/components/Dictation.tsx";
import type { Recognition } from "../../src/components/BrowserDictation.tsx";
import "../../src/components/realtime-voice.css";

declare global {
  interface Window {
    dictationCalls: string[];
    speech: Recognition | undefined;
    SpeechRecognition?: (new () => Recognition) | undefined;
    webkitSpeechRecognition?: (new () => Recognition) | undefined;
  }
}
window.dictationCalls = [];
window.SpeechRecognition = class implements Recognition {
  declare lang: string;
  declare continuous: boolean;
  declare interimResults: boolean;
  declare onresult: Recognition["onresult"];
  declare onerror: Recognition["onerror"];
  declare onstart: Recognition["onstart"];
  declare onend: Recognition["onend"];
  constructor() {
    window.speech = this;
  }
  start() {
    window.dictationCalls.push("start");
    this.onstart?.();
  }
  stop() {
    window.dictationCalls.push("stop");
    this.onend?.();
  }
  abort() {
    window.dictationCalls.push("abort");
    this.onend?.();
  }
};
if (location.search.includes("unsupported")) {
  window.SpeechRecognition = undefined;
  window.webkitSpeechRecognition = undefined;
}
function App() {
  const [draft, setDraft] = useState("");
  const [chat, setChat] = useState("a");
  return (
    <MantineProvider>
      <button onClick={() => setChat(chat === "a" ? "b" : "a")}>
        Switch chat
      </button>
      <textarea aria-label="Draft" value={draft} readOnly />
      <Dictation key={chat} chatId={chat} onInsert={setDraft} />
    </MantineProvider>
  );
}
const root = document.getElementById("root")!;
createRoot(root).render(<App />);
