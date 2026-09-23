import type { Snapshot } from "./types";

export type DesktopAlert = {
  id: string;
  title: string;
  body: string;
  target: { agentId: string; section: "messages"; itemId?: string };
};

export function desktopAlerts(data: Snapshot): DesktopAlert[] {
  const agents = new Map(data.threads.map((agent) => [agent.id, agent]));
  const alerts: DesktopAlert[] = [];
  const add = (
    id: string,
    agentId: string,
    title: string,
    body: string,
    itemId?: string,
  ) => {
    const agent = agents.get(agentId);
    if (!agent || agent.deletedAt) return;
    alerts.push({
      id,
      title: `${agent.name}: ${title}`.slice(0, 160),
      body: body.slice(0, 2000),
      target: { agentId, section: "messages", ...(itemId ? { itemId } : {}) },
    });
  };
  for (const agent of agents.values()) {
    if (!agent.isLead || agent.inFlight) continue;
    const turn = agent.lastCompletedTurn;
    if (
      agent.status === "completed" &&
      turn &&
      agent.lastCompletedTurnStatus === "completed"
    )
      add(
        `completed:${agent.id}:${turn}`,
        agent.id,
        "Reply ready",
        agent.tail || "Open the chat to read the reply.",
      );
    if (agent.status === "failed" || agent.status === "interrupted")
      add(
        `failed:${agent.id}:${turn || agent.turnId || agent.status}`,
        agent.id,
        "Work stopped",
        typeof agent.error === "string"
          ? agent.error
          : agent.error &&
              typeof agent.error === "object" &&
              "message" in agent.error &&
              typeof agent.error.message === "string"
            ? agent.error.message
            : "Open the chat to check the error.",
      );
  }
  for (const request of data.runtime.requests || []) {
    if (request.status !== "pending" || request.deferred) continue;
    const question = request.params?.questions?.[0]?.question;
    add(
      `request:${request.id}`,
      request.agent,
      question ? "Question for you" : "Your approval is needed",
      question || "Open the request to review it.",
      request.id,
    );
  }
  for (const complaint of data.runtime.complaints || []) {
    if (
      complaint.needsResponse &&
      (complaint.recipient === "user" ||
        (!complaint.recipient && complaint.author === complaint.leadId))
    )
      add(
        `complaint:${complaint.id}:${complaint.version || 0}`,
        complaint.leadId,
        "Message for you",
        complaint.title,
        complaint.id,
      );
  }
  return alerts;
}
