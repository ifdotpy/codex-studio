import { describe, expect, it } from "vitest";
import type { GetResult } from "../api";
import { agentChatMessages } from "./agentChatMessages";

type AgentChatRecord = GetResult<"/api/agent-chat">["messages"][number];

describe("agent chat API normalization", () => {
  it("maps generated room rows to assistant messages without losing identity or order", () => {
    const rows: AgentChatRecord[] = [
      {
        id: "message-1",
        room: "room-1",
        sender: "agent-1",
        senderName: "Worker",
        seq: 17,
        text: "A room reply",
        created: 1234,
        deliveries: { lead: "delivered" },
      },
    ];

    expect(agentChatMessages(rows)).toEqual([
      {
        id: "message-1",
        text: "A room reply",
        role: "assistant",
        kind: "message",
        at: 1234,
        created: 1234,
        seq: 17,
        sender: "agent-1",
        senderName: "Worker",
        sourceId: "agent-1",
      },
    ]);
  });
});
