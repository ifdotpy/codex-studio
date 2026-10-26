import type { Message } from "../../src/types.ts";
import { required } from "./browser-helpers.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import StreamingText from "../../src/components/conversation/transcript/StreamingText.tsx";
import ConversationResults from "../../src/components/conversation/transcript/ConversationResults.tsx";
import { completedMarkdown } from "../../src/native/markdownCache.ts";
window.markdownCache = completedMarkdown;
window.markdownCompilations = 0;
const save = completedMarkdown.set.bind(completedMarkdown);
completedMarkdown.set = (...args) => {
  window.markdownCompilations++;
  return save(...args);
};
const initial = Array.from({ length: 2 }, (_, i) => ({
  id: "reply-" + i,
  text:
    "Reply " +
    i +
    ". Completed **text** with [report][ref].\n\n[ref]: ./report.md",
}));
const other = initial.map((item) => ({
  ...item,
  id: "other-" + item.id,
  text: "Other chat " + item.text,
}));
function Harness() {
  const [items, setItems] =
      useState<
        Array<Pick<Message, "id" | "text"> & Partial<Pick<Message, "role">>>
      >(initial),
    [shown, setShown] = useState(true),
    [agent, setAgent] = useState("first"),
    [streaming, setStreaming] = useState(false),
    [results, setResults] = useState(false);
  window.markdownResults = () => flushSync(() => setResults(true));
  window.markdownChat = (otherChat) =>
    flushSync(() => setItems(otherChat ? other : initial));
  window.markdownHide = () => flushSync(() => setShown(false));
  window.markdownShow = () => flushSync(() => setShown(true));
  window.markdownAgent = (id) => flushSync(() => setAgent(id));
  window.markdownText = (text, stream = false) =>
    flushSync(() => {
      setItems([{ id: "only", role: "assistant", text }]);
      setStreaming(stream);
    });
  return (
    <MantineProvider>
      <>
        {shown &&
          items.map((item) => (
            <article key={item.id}>
              <StreamingText
                text={item.text}
                streaming={streaming}
                agentId={agent}
              />
            </article>
          ))}
      </>
      {results && (
        <ConversationResults
          messages={[
            {
              id: "patch",
              role: "output",
              toolStatus: "completed",
              text: JSON.stringify({
                type: "fileChange",
                status: "completed",
                changes: [
                  { path: "file.txt", diff: "@@ -1 +1 @@\n-old\n+saved" },
                ],
              }),
            },
          ]}
          agentId={agent}
        />
      )}
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<Harness />);
declare global {
  interface Window {
    markdownCache: typeof completedMarkdown;
    markdownCompilations: number;
    markdownResults: () => void;
    markdownChat: (otherChat: boolean) => void;
    markdownHide: () => void;
    markdownShow: () => void;
    markdownAgent: (id: string) => void;
    markdownText: (text: string, stream?: boolean) => void;
  }
}
