import { describe, expect, it } from "vitest";
import { matchingCompletedAgent, type ChatReadProof } from "./useChatReadState";
import type { Agent } from "../types";

describe("read-state entity reconciliation", () => {
  it("reads the saved read state from the matching completed agent entity", () => {
    const proof: ChatReadProof = {
      id: "agent-a",
      threadId: "thread-a",
      turnId: "turn-a",
    };
    const agent = {
      id: proof.id,
      threadId: proof.threadId,
      lastCompletedTurn: proof.turnId,
      lastCompletedTurnStatus: "completed",
      readStateSupported: true,
      readState: {
        threadId: proof.threadId,
        turnId: proof.turnId,
        read: true,
        revision: 3,
      },
    } satisfies Agent;

    expect(matchingCompletedAgent([agent], proof)).toBe(agent);
    expect(matchingCompletedAgent([agent], { ...proof, turnId: "stale" })).toBe(
      null,
    );
  });
});
