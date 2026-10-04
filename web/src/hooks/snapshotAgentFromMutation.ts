import type { PostResult } from "../api";
import type { Agent } from "../types";

type AgentMutationResponse = PostResult<"/api/leads">;

/** Add local agent classification needed to display an agent-only mutation. */
export function snapshotAgentFromMutation(
  response: AgentMutationResponse,
): Agent {
  return {
    ...response,
    kind: "agent",
    source: "managed",
    canSend: response.canSend ?? !response.threadId,
  };
}
