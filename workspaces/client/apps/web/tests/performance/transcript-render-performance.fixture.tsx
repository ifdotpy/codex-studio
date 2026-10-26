import { required } from "./performance-fixture.ts";
import type { Message } from "../../src/types.ts";
import React, { useState, Profiler } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import { marked } from "marked";
import DOMPurify from "dompurify";
import TurnHistory from "../../src/components/conversation/transcript/TurnHistory.tsx";
import StreamingText from "../../src/components/conversation/transcript/StreamingText.tsx";
import { NativeNotice } from "../../src/components/NativeNotice.tsx";
const counters = {
  lex: 0,
  lexChars: 0,
  sanitize: 0,
  sanitizeChars: 0,
  toolParse: 0,
  renders: 0,
  duration: 0,
};
window.transcriptPerformanceCounters = counters;
const lex = marked.lexer;
marked.lexer = new Proxy(lex, {
  apply(target, thisArg, args: Parameters<typeof lex>) {
    counters.lex++;
    counters.lexChars += args[0].length;
    return Reflect.apply(target, thisArg, args);
  },
});
const sanitize = DOMPurify.sanitize;
DOMPurify.sanitize = new Proxy(sanitize, {
  apply(target, thisArg, args: Parameters<typeof sanitize>) {
    counters.sanitize++;
    counters.sanitizeChars += typeof args[0] === "string" ? args[0].length : 0;
    return Reflect.apply(target, thisArg, args);
  },
});
const parse = JSON.parse;
JSON.parse = function (t, ...args) {
  if (typeof t === "string" && t.includes("audit_tool_payload"))
    counters.toolParse++;
  return parse.call(this, t, ...args);
};
const prose = Array.from(
  { length: 10 },
  (_, i) =>
    "Paragraph " +
    i +
    ". " +
    "Text with **strong words** and [source](./src/main.ts). ".repeat(12),
).join("\n\n");
const tooltext = JSON.stringify({
  type: "commandExecution",
  status: "completed",
  command: "audit_tool_payload",
  aggregatedOutput: "x".repeat(8192),
});
const initial: Message[] = Array.from({ length: 30 }, (_, i) => [
  {
    id: "u" + i,
    role: "user",
    text: "Question",
    turnId: "t" + i,
    turnStatus: "completed",
  },
  ...Array.from({ length: 3 }, (_, j) => ({
    id: "tool" + i + "-" + j,
    role: "output",
    text: tooltext,
    turnId: "t" + i,
    turnStatus: "completed",
    toolStatus: "completed",
  })),
  {
    id: "a" + i,
    role: "assistant",
    text: prose,
    turnId: "t" + i,
    turnStatus: "completed",
    phase: "final_answer",
  },
]).flat();
const render = (m: Message) =>
  m.nativeNotice ? (
    <NativeNotice item={m} />
  ) : (
    <StreamingText text={m.text} streaming={!!m.streaming} agentId="fixture" />
  );
const jump = () => {};
function Harness() {
  const [tick, setTick] = useState(0);
  const [mode, setMode] = useState("history");
  const [suffix, setSuffix] = useState("");
  const [items, setItems] = useState<Message[]>(initial);
  const [stream, setStream] = useState<string | null>(null);
  window.transcriptPerformanceReplaceLast = () =>
    flushSync(() =>
      setItems((old) =>
        old.map((m) =>
          m.id === "a29"
            ? { ...m, text: m.text + "\n\n[Updated report](./changed.md)" }
            : m,
        ),
      ),
    );
  window.transcriptPerformanceDiagnostic = (
    error: NonNullable<Message["turnError"]>,
  ) =>
    flushSync(() =>
      setItems((old) =>
        old.map((m) =>
          m.id === "a29" ? { ...m, turnStatus: "failed", turnError: error } : m,
        ),
      ),
    );
  window.transcriptPerformanceStream = (text: string) =>
    flushSync(() => setStream(text));
  window.transcriptPerformanceTick = () =>
    flushSync(() => setTick((v) => v + 1));
  window.transcriptPerformanceMode = (v: string) => flushSync(() => setMode(v));
  window.transcriptPerformanceAppend = () =>
    flushSync(() => setSuffix((v) => v + " Next sentence."));
  return (
    <MantineProvider>
      <input value={tick} readOnly />
      <Profiler
        id="history"
        onRender={(_id, _phase, d) => {
          counters.renders++;
          counters.duration += d;
        }}
      >
        {mode === "history" ? (
          <TurnHistory
            items={items.slice()}
            enabled
            storageKey="audit-only"
            renderMessage={(m) => render(m)}
            onJump={jump}
          />
        ) : (
          <StreamingText
            text={stream ?? prose + suffix}
            streaming
            agentId="fixture"
          />
        )}
      </Profiler>
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Harness />);

declare global {
  interface Window {
    transcriptPerformanceCounters: typeof counters;
    transcriptPerformanceReplaceLast: () => void;
    transcriptPerformanceDiagnostic: (
      error: NonNullable<Message["turnError"]>,
    ) => void;
    transcriptPerformanceStream: (text: string) => void;
    transcriptPerformanceTick: () => void;
    transcriptPerformanceMode: (mode: string) => void;
    transcriptPerformanceAppend: () => void;
  }
}
