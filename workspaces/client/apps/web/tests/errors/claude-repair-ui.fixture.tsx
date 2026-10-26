import type { Agent, Message } from "../../src/types.ts";
import { required } from "../teams/helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import TurnHistory from "../../src/components/conversation/transcript/TurnHistory.tsx";
import ClaudeRepairNotice from "../../src/components/ClaudeRepairNotice.tsx";
import { NativeNotice } from "../../src/components/NativeNotice.tsx";
const agent: Agent = {
  id: "agent",
  threadId: "thread",
  provider: "claude",
  status: "paused",
  epoch: 1,
  error: "CLAUDE_SESSION_REPAIR_REQUIRED: needs repair",
};
const items: Message[] = [
  {
    id: "input",
    role: "user",
    text: "Update the report.",
    turnId: "turn",
    turnStatus: "uncertain",
  },
  {
    id: "answer",
    role: "assistant",
    text: "I read the source files.",
    turnId: "turn",
    turnStatus: "uncertain",
    turnError: {
      message:
        "Studio could not confirm the Claude turn outcome. The native transcript is incomplete.",
    },
  },
];
createRoot(required(document.getElementById("root"))).render(
  React.createElement(
    MantineProvider,
    null,
    React.createElement(
      "main",
      { style: { maxWidth: 900, margin: "auto", padding: 12 } },
      React.createElement("h1", null, "Conversation"),
      React.createElement(ClaudeRepairNotice, { agent }),
      React.createElement(TurnHistory, {
        items,
        agent,
        agentId: "agent",
        enabled: true,
        storageKey: "repair-fixture",
        onJump: () => {},
        renderMessage: (item) =>
          item.nativeNotice
            ? React.createElement(NativeNotice, { item })
            : React.createElement("p", null, item.text),
      }),
    ),
  ),
);
