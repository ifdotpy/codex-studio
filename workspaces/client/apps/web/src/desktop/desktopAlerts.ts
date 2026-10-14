import type { Snapshot } from "../types";

export type DesktopAlert = {
  id: string;
  title: string;
  body: string;
  target: { agentId: string; section: "messages"; itemId?: string };
  /** When the event happened (seconds), when the record says. */
  at?: number;
};

// A window that starts or reloads takes the alerts it finds as already known. Records that
// arrive after that first snapshot (a slow page of the sync pull) are not new events: an alert
// whose event is older than the start of the window is never shown. The margin covers a clock
// difference between paired Macs.
const START_MARGIN_SECONDS = 120;
export function alertsSince(started: number) {
  const since = started / 1000 - START_MARGIN_SECONDS;
  return (alert: DesktopAlert) => alert.at === undefined || alert.at >= since;
}

export function desktopAlerts(data: Snapshot): DesktopAlert[] {
  const agents = new Map(data.threads.map((agent) => [agent.id, agent]));
  const alerts: DesktopAlert[] = [];
  const add = (
    id: string,
    agentId: string | null | undefined,
    title: string,
    body: string,
    itemId?: string,
    at?: number | null,
  ) => {
    const agent = typeof agentId === "string" ? agents.get(agentId) : undefined;
    if (!agent || agent.deletedAt) return;
    alerts.push({
      id,
      title: `${agent.name}: ${title}`.slice(0, 160),
      body: body.slice(0, 2000),
      target: {
        agentId: agent.id,
        section: "messages",
        ...(itemId ? { itemId } : {}),
      },
      ...(typeof at === "number" ? { at } : {}),
    });
  };
  for (const agent of agents.values()) {
    if (
      !agent.isLead ||
      agent.inFlight ||
      (agent as typeof agent & { remoteAnchor?: unknown; movedTo?: unknown })
        .remoteAnchor ||
      agent.movedTo ||
      agent.moveImportPending
    )
      continue;
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
  for (const request of data.runtime.requests) {
    if (request.status !== "pending" || request.deferred) continue;
    const question = request.params?.questions?.[0]?.question;
    add(
      `request:${request.id}`,
      request.agent,
      question ? "Question for you" : "Your approval is needed",
      question || "Open the request to review it.",
      request.id,
      request.createdAt ?? request.created ?? request.at,
    );
  }
  for (const complaint of data.runtime.complaints) {
    if (complaint.needsUserResponse)
      add(
        `complaint:${complaint.id}:${complaint.version || 0}`,
        complaint.leadId,
        "Message for you",
        complaint.title || "Open the message to read it.",
        complaint.id,
        complaint.updated ?? complaint.created,
      );
  }
  return alerts;
}
