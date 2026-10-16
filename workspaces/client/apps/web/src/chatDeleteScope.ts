import type { Agent } from "./types";
import { ApiError } from "./api";
import { serverMutationResponseIsUncertain } from "./servers/mutationIdentity";

export type PendingConversationDelete = {
  targetId: string;
  scope: Set<string>;
  stateDir: string;
  workspaceId: string;
  phase: "submitting" | "checking" | "readback";
};

/** Return the target agent and every descendant that a conversation delete stops. */
export function agentDeleteScope(
  agents: readonly Agent[],
  targetId: string,
): Set<string> {
  const scope = new Set([targetId]);
  let changed = true;
  while (changed) {
    changed = false;
    for (const agent of agents) {
      if (agent.parentId && scope.has(agent.parentId) && !scope.has(agent.id)) {
        scope.add(agent.id);
        changed = true;
      }
    }
  }
  return scope;
}

export function beginConversationDelete(
  targetId: string,
  agents: readonly Agent[],
  stateDir: string,
  workspaceId: string,
  suspendReaders: (scope: ReadonlySet<string>) => void,
): PendingConversationDelete {
  const scope = agentDeleteScope(agents, targetId);
  suspendReaders(scope);
  return { targetId, scope, stateDir, workspaceId, phase: "submitting" };
}

export function withoutAgentDeleteScope(
  agents: readonly Agent[],
  scope: ReadonlySet<string>,
): Agent[] {
  return agents.filter((agent) => !scope.has(agent.id));
}

export function claimConversationDeletePost(
  pending: PendingConversationDelete,
  started: WeakSet<PendingConversationDelete>,
): boolean {
  if (pending.phase !== "submitting" || started.has(pending)) return false;
  started.add(pending);
  return true;
}

export function definiteDeleteRejection(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    error.status < 500 &&
    !error.outcome &&
    !serverMutationResponseIsUncertain(error.status, error.response)
  );
}

export function deleteScopeConfirmedGone(
  agents: readonly Agent[],
  scope: ReadonlySet<string>,
): boolean {
  return [...scope].every((id) => {
    const current = agents.find((agent) => agent.id === id);
    return !current || !!current.deletedAt;
  });
}

export function conversationDeleteReadback(
  agents: readonly Agent[] | null,
  scope: ReadonlySet<string>,
  expected: Pick<PendingConversationDelete, "stateDir" | "workspaceId">,
  stateDir: string,
  workspaceId: string,
): "confirmed" | "still-present" | "unavailable" {
  if (
    !agents ||
    stateDir !== expected.stateDir ||
    workspaceId !== expected.workspaceId
  )
    return "unavailable";
  return deleteScopeConfirmedGone(agents, scope)
    ? "confirmed"
    : "still-present";
}

export function confirmedConversationDeleteIds(
  value: unknown,
  targetId: string,
  scope: ReadonlySet<string>,
): string[] | null {
  if (
    !Array.isArray(value) ||
    !value.every((id): id is string => typeof id === "string") ||
    !value.includes(targetId) ||
    !value.every((id) => scope.has(id)) ||
    ![...scope].every((id) => value.includes(id))
  )
    return null;
  return [...new Set(value)];
}
