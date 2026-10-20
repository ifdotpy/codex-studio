import { useCallback, useSyncExternalStore } from "react";
import {
  chatActivities,
  chatChildren,
  chatWaitState,
  type ChatWaitState,
} from "../components/chat-status/chatStatusModel";
import type { Agent, Project, Request, Snapshot } from "../types";
import { retainArray } from "./snapshotSelection";

/**
 * The agent fields an open conversation reads from its team. Status and
 * activity fields are absent on purpose: a worker's progress must not render
 * the transcript or the composer. The conversation learns about progress
 * through `ChatWaitSource`, which only notifies when the lead's wait changes.
 */
export type ConversationAgent = Pick<
  Agent,
  | "id"
  | "name"
  | "rootId"
  | "deletedAt"
  | "accountKey"
  | "threadId"
  | "nativeThreadBlock"
  | "error"
>;

/** The snapshot parts a conversation reads, each with a retained identity. */
export type ConversationData = {
  stateDir: string;
  threads: ConversationAgent[];
  runtime: {
    requests: Request[];
    agents: ConversationAgent[];
    projects: Project[];
    nativeNotices: Snapshot["runtime"]["nativeNotices"];
  };
};

function pickAgent(agent: Agent): ConversationAgent {
  return {
    id: agent.id,
    name: agent.name,
    rootId: agent.rootId,
    deletedAt: agent.deletedAt,
    accountKey: agent.accountKey,
    threadId: agent.threadId,
    nativeThreadBlock: agent.nativeThreadBlock,
    error: agent.error,
  };
}

function sameAgent(a: ConversationAgent, b: ConversationAgent): boolean {
  return (
    a.name === b.name &&
    a.rootId === b.rootId &&
    a.deletedAt === b.deletedAt &&
    a.accountKey === b.accountKey &&
    a.threadId === b.threadId &&
    a.nativeThreadBlock === b.nativeThreadBlock &&
    a.error === b.error
  );
}

// Keep each picked row until a picked field changes, then keep the array.
function agentProjection() {
  let input: Agent[] | undefined;
  let rows = new Map<string, ConversationAgent>();
  let output: ConversationAgent[] = [];
  return (source: Agent[]): ConversationAgent[] => {
    if (input === source) return output;
    input = source;
    const next = new Map<string, ConversationAgent>();
    const picked = source.map((agent) => {
      const row = pickAgent(agent);
      const previous = rows.get(agent.id);
      const kept = previous && sameAgent(previous, row) ? previous : row;
      next.set(agent.id, kept);
      return kept;
    });
    rows = next;
    output = retainArray(output, picked);
    return output;
  };
}

export function createConversationDataSelector() {
  const threads = agentProjection();
  const agents = agentProjection();
  let previous: ConversationData | null = null;
  return (data: Snapshot | null): ConversationData | null => {
    if (!data) return null;
    const runtime = {
      requests: data.runtime.requests,
      agents: agents(data.runtime.agents),
      projects: data.runtime.projects,
      nativeNotices: data.runtime.nativeNotices,
    };
    const next: ConversationData = {
      stateDir: data.stateDir,
      threads: threads(data.threads),
      runtime:
        previous &&
        previous.runtime.requests === runtime.requests &&
        previous.runtime.agents === runtime.agents &&
        previous.runtime.projects === runtime.projects &&
        previous.runtime.nativeNotices === runtime.nativeNotices
          ? previous.runtime
          : runtime,
    };
    previous =
      previous &&
      previous.stateDir === next.stateDir &&
      previous.threads === next.threads &&
      previous.runtime === next.runtime
        ? previous
        : next;
    return previous;
  };
}

function sameActivities<T extends object>(a: T[], b: T[]): boolean {
  return (
    a.length === b.length &&
    a.every((item, index) =>
      Object.entries(item).every(
        ([key, value]) => value === (b[index] as Record<string, unknown>)[key],
      ),
    )
  );
}

function sameWait(a: ChatWaitState, b: ChatWaitState): boolean {
  return (
    a.live === b.live &&
    a.label === b.label &&
    a.event === b.event &&
    a.inputs === b.inputs &&
    a.agents.length === b.agents.length &&
    a.agents.every((agent, index) => agent === b.agents[index]) &&
    sameActivities(a.commands, b.commands) &&
    sameActivities(a.monitors, b.monitors)
  );
}

/**
 * Publishes the current snapshot to conversations without rendering them.
 * `wait` returns one identity until the wait itself changes, so a worker-only
 * snapshot notifies subscribers and renders none of them.
 */
export class ChatWaitSource {
  private latest: Snapshot | null = null;
  private listeners = new Set<() => void>();
  // The O(agents) indexes are shared by every subscriber of one snapshot.
  private shared: {
    data: Snapshot;
    activities: ReturnType<typeof chatActivities>;
    children: ReturnType<typeof chatChildren>;
  } | null = null;
  private byAgent = new WeakMap<
    Agent,
    { data: Snapshot; wait: ChatWaitState }
  >();
  private lastById = new Map<string, ChatWaitState>();

  /** Record the snapshot during render so children read a consistent value. */
  set(data: Snapshot | null) {
    this.latest = data;
  }

  /** Call after commit: subscribers re-read and render only if their wait changed. */
  publish() {
    for (const listener of this.listeners) listener();
  }

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  wait(agent: Agent | undefined): ChatWaitState | undefined {
    const data = this.latest;
    if (!data || !agent) return undefined;
    const cached = this.byAgent.get(agent);
    if (cached?.data === data) return cached.wait;
    if (this.shared?.data !== data)
      this.shared = {
        data,
        activities: chatActivities(data),
        children: chatChildren(data),
      };
    const computed = chatWaitState(
      data,
      agent,
      this.shared.activities,
      this.shared.children,
    );
    const previous = this.lastById.get(agent.id);
    const wait = previous && sameWait(previous, computed) ? previous : computed;
    this.lastById.set(agent.id, wait);
    this.byAgent.set(agent, { data, wait });
    return wait;
  }
}

export function useChatWait(source: ChatWaitSource, agent: Agent | undefined) {
  return useSyncExternalStore(
    source.subscribe,
    useCallback(() => source.wait(agent), [source, agent]),
  );
}
