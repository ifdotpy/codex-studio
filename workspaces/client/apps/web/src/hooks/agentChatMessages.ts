import type { GetResult } from "../api";
import type { Message } from "../types";

type AgentChatRecord = GetResult<"/api/agent-chat">["messages"][number];

export function agentChatMessages(
  records: readonly AgentChatRecord[],
): Message[] {
  return records.map((record) => ({
    id: record.id,
    text: record.text,
    role: "assistant",
    kind: "message",
    at: record.created,
    created: record.created,
    seq: record.seq,
    sender: record.sender,
    senderName: record.senderName,
    sourceId: record.sender,
  }));
}
