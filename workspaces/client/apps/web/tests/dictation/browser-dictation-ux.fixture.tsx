import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import { Dictation } from "../../src/components/Dictation.tsx";
import type { Recognition } from "../../src/components/BrowserDictation.tsx";
import "../../src/components/realtime-voice.css";

declare global {
  interface Window {
    browserDictationFixture: { calls: string[]; speech?: Recognition };
    SpeechRecognition?: (new () => Recognition) | undefined;
    webkitSpeechRecognition?: (new () => Recognition) | undefined;
  }
}
window.browserDictationFixture = { calls: [] };
window.SpeechRecognition = class implements Recognition {
  lang = "";
  continuous = false;
  interimResults = false;
  onresult: Recognition["onresult"] = null;
  onerror: Recognition["onerror"] = null;
  onstart: Recognition["onstart"] = null;
  onend: Recognition["onend"] = null;
  constructor() {
    window.browserDictationFixture.speech = this;
  }
  start() {
    window.browserDictationFixture.calls.push("start");
    this.onstart?.();
  }
  stop() {
    window.browserDictationFixture.calls.push("stop");
    this.onend?.();
  }
  abort() {
    window.browserDictationFixture.calls.push("abort");
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
const root = document.getElementById("root");
if (!root) throw new Error("Missing browser dictation root");
createRoot(root).render(<App />);
