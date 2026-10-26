import type { Agent } from "../../src/types.ts";
export const disconnected =
  "Codex disconnected. Review the transcript before resuming.";
export const restarted =
  "Server restarted during a turn. Review history, then send a new instruction.";
export const original = {
  id: "first",
  source: "managed",
  status: "interrupted",
  inFlight: false,
  threadId: "thread-first",
  turnId: "turn-first",
  epoch: 1,
  accountKey: "default",
  error: disconnected,
} satisfies Agent;
