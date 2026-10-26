import { required } from "./performance-fixture.ts";
import type { Message } from "../../src/types.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { MantineProvider } from "@mantine/core";
import TurnHistory from "../../src/components/conversation/transcript/TurnHistory.tsx";
import StreamingText from "../../src/components/conversation/transcript/StreamingText.tsx";

const initial: Message[] = [
  {
    id: "user",
    role: "user",
    text: "Run the command",
    turnId: "complete-turn",
    turnStatus: "completed",
  },
  {
    id: "failed-command",
    role: "output",
    title: "Command finished",
    toolStatus: "completed",
    turnId: "complete-turn",
    turnStatus: "completed",
    text: JSON.stringify({
      type: "commandExecution",
      status: "completed",
      exitCode: 0,
    }),
  },
  {
    id: "answer",
    role: "assistant",
    text: "The task is complete.",
    turnId: "complete-turn",
    turnStatus: "completed",
    phase: "final_answer",
  },
];

function App() {
  const [items, setItems] = useState<Message[]>(initial);
  window.failedToolFailCompletedTurnTool = () =>
    flushSync(() =>
      setItems((old) =>
        old.map((item) =>
          item.id === "failed-command"
            ? {
                ...item,
                toolStatus: "failed",
                text: JSON.stringify({
                  type: "commandExecution",
                  status: "failed",
                  error: {
                    message: "Usage limit reached",
                    codexErrorInfo: "usageLimitExceeded",
                  },
                }),
              }
            : item,
        ),
      ),
    );
  return (
    <MantineProvider>
      <TurnHistory
        items={items}
        enabled
        storageKey="completed-turn-limit"
        renderMessage={(item) => (
          <StreamingText text={item.text} streaming={false} agentId="fixture" />
        )}
        onJump={() => {}}
      />
    </MantineProvider>
  );
}

createRoot(required(document.getElementById("root"))).render(<App />);
declare global {
  interface Window {
    failedToolFailCompletedTurnTool: () => void;
  }
}
