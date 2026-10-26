import type { Attachment } from "../../src/components/ComposerAttachments";
import type { Message } from "../../src/types";
declare global {
  interface Window {
    setText: (value: string) => void;
    setCount: (value: number) => void;
    setStreaming: (value: boolean) => void;
    setTheme: (value: "light" | "dark") => void;
    retained: Element;
  }
}

import { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import StreamingText from "../../src/components/conversation/transcript/StreamingText";
import Activity from "../../src/components/conversation/transcript/Activity";
import TurnHistory from "../../src/components/conversation/transcript/TurnHistory";
import ComposerAttachments, {
  MessageAttachments,
} from "../../src/components/ComposerAttachments";
const asset = {
  id: "fixture-image",
  name: "pixel.png",
  mime: "image/png",
  image: true,
  size: 68,
  preview:
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aN1sAAAAASUVORK5CYII=",
} satisfies Attachment;
function Harness() {
  const [text, setText] = useState("First sentence. Partial");
  const [count, setCount] = useState(2);
  const [streaming, setStreaming] = useState(true);
  const [theme, setTheme] = useState<"light" | "dark">("light");
  Object.assign(window, { setText, setCount, setStreaming, setTheme });
  const items: Message[] = Array.from({ length: count }, (_, i) => ({
    id: "tool" + i,
    turnId: "turn",
    role: "tool",
    text: JSON.stringify({
      type: "commandExecution",
      command: "pwd",
      aggregatedOutput: "result",
    }),
    createdAt: "2026-09-10",
  }));
  return (
    <MantineProvider forceColorScheme={theme}>
      <main>
        <div id="text">
          <StreamingText text={text} streaming={streaming} agentId="fixture" />
        </div>
        <div id="activity">
          <Activity items={items} />
        </div>
        <div id="turn">
          <TurnHistory
            items={items}
            currentTurn="turn"
            enabled
            storageKey="test-rendering"
            renderMessage={() => null}
            onJump={() => {}}
          />
        </div>
        <ComposerAttachments
          notify={() => {}}
          assets={[asset]}
          uploading={false}
          disabled={false}
          add={async () => {}}
          remove={() => {}}
        />
        <MessageAttachments assets={[asset]} notify={() => {}} />
      </main>
    </MantineProvider>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing rendering fixture root");
createRoot(root).render(<Harness />);
