import type { Agent, Room, Snapshot } from "./types";

// Private rooms predate rootId metadata. Their participants define the scope.
// Historical cross-team rooms remain visible to the user in the participant chats.
// Agent position by ID, built once per agents array: a snapshot asks this for
// every room, so a per-room scan of all agents would scale with rooms x agents.
const agentPositions = new WeakMap<Agent[], Map<string, number>>();

export function roomLeadIds(room: Room, agents: Agent[]): string[] {
  if (room.kind === "broadcast")
    return room.rootId && room.rootId !== "all" ? [room.rootId] : [];
  let positions = agentPositions.get(agents);
  if (!positions) {
    positions = new Map();
    agents.forEach((agent, index) => positions!.set(agent.id, index));
    agentPositions.set(agents, positions);
  }
  // Agent order decides which lead is the room's default.
  const participants = [...new Set(room.members)]
    .map((id) => positions.get(id))
    .filter((index): index is number => index !== undefined)
    .sort((a, b) => a - b);
  return [
    ...new Set(
      participants
        .map((index) => agents[index]!)
        .map((agent) => (agent.isLead ? agent.id : agent.rootId))
        .filter((id): id is string => !!id),
    ),
  ];
}

// A conversation belongs to one lead tree, even when two chats use the same folder.
// A missing selection must never expand into all sessions.
export function chatSnapshot(
  data: Snapshot | null,
  rootId?: string,
): Snapshot | null {
  if (!data) return null;
  const runtime = data.runtime;
  const members = data.threads.filter(
    (agent) => !!rootId && (agent.id === rootId || agent.rootId === rootId),
  );
  const ids = new Set(members.map((agent) => agent.id));
  const owns = (id?: string) => !!id && ids.has(id);
  return {
    ...data,
    threads: members,
    chats: [],
    runtime: {
      ...runtime,
      agents: runtime.agents.filter((agent) => owns(agent.id)),
      rooms: runtime.rooms.filter(
        (room) => !!rootId && roomLeadIds(room, data.threads).includes(rootId),
      ),
      requests: runtime.requests.filter((request) =>
        owns(request.agent || undefined),
      ),
      complaints: runtime.complaints.filter(
        (complaint) => !!rootId && complaint.leadId === rootId,
      ),
      monitors: runtime.monitors.filter((monitor) =>
        owns(monitor.agent || undefined),
      ),
      tasks: runtime.tasks.filter((task) => owns(task.agent || undefined)),
      work: runtime.work.filter((task) => !!rootId && task.rootId === rootId),
      rules: runtime.rules.filter((rule) => owns(rule.agent || undefined)),
    },
  };
}

export function messageAttentionCount(data: Snapshot | null): number {
  if (!data) return 0;
  return (
    data.runtime.requests.filter((request) => !request.deferred).length +
    data.runtime.complaints.filter((complaint) => complaint.needsUserResponse)
      .length
  );
}
