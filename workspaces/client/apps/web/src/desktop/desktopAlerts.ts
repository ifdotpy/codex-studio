import {
  SIGN_IN_ALERT_GRACE_SECONDS,
  SIGN_IN_REMINDER_SECONDS,
} from "../generated/sharedValues";
import type { Snapshot } from "../types";

export type DesktopAlert = {
  id: string;
  title: string;
  body: string;
  target: { agentId: string; section: "messages"; itemId?: string };
  /** When the event happened (seconds), when the record says. */
  at?: number;
  /** The alert plays the system sound: a sign-in that waits for the person. */
  sound?: boolean;
};

// The grace and the reminder lead have one owner, runtime-schema shared_values.rs: the phone
// pushes of the server use the same times.
export {
  SIGN_IN_ALERT_GRACE_SECONDS,
  SIGN_IN_REMINDER_SECONDS,
} from "../generated/sharedValues";
const SIGN_IN_METHOD = "browser/open";

type SignInTimes = { host: string; alertAt: number; reminderAt: number | null };

/** The times of a pending sign-in card's alert and reminder, in seconds, or null. */
function signInTimes(
  request: Snapshot["runtime"]["requests"][number],
): SignInTimes | null {
  if (
    request.method !== SIGN_IN_METHOD ||
    request.status !== "pending" ||
    request.deferred
  )
    return null;
  const params = (request.params ?? {}) as {
    host?: unknown;
    expiresAt?: unknown;
    phase?: unknown;
  };
  const created = request.createdAt ?? request.created ?? request.at;
  if (typeof created !== "number") return null;
  const alertAt = created + SIGN_IN_ALERT_GRACE_SECONDS;
  const expiresAt =
    typeof params.expiresAt === "number" ? params.expiresAt : null;
  // A phone sign-in that waits for its pasted result already has the person.
  const reminderAt =
    expiresAt !== null && params.phase !== "waiting"
      ? expiresAt - SIGN_IN_REMINDER_SECONDS
      : null;
  return {
    host: typeof params.host === "string" ? params.host : "the sign-in page",
    alertAt,
    reminderAt: reminderAt !== null && reminderAt > alertAt ? reminderAt : null,
  };
}

/**
 * The next moment, in seconds, at which a sign-in alert or reminder becomes due, or null.
 * The alert hooks wake at that moment, because no record changes then.
 */
export function nextAlertChange(data: Snapshot, now: number): number | null {
  let next: number | null = null;
  for (const request of data.runtime.requests) {
    const times = signInTimes(request);
    if (!times) continue;
    for (const at of [times.alertAt, times.reminderAt]) {
      if (at !== null && at > now && (next === null || at < next)) next = at;
    }
  }
  return next;
}

// A window that starts or reloads takes the alerts it finds as already known. Records that
// arrive after that first snapshot (a slow page of the sync pull) are not new events: an alert
// whose event is older than the start of the window is never shown. The margin covers a clock
// difference between paired Macs.
const START_MARGIN_SECONDS = 120;
export function alertsSince(started: number) {
  const since = started / 1000 - START_MARGIN_SECONDS;
  return (alert: DesktopAlert) => alert.at === undefined || alert.at >= since;
}

export function desktopAlerts(
  data: Snapshot,
  now = Date.now() / 1000,
): DesktopAlert[] {
  const agents = new Map(data.threads.map((agent) => [agent.id, agent]));
  const alerts: DesktopAlert[] = [];
  const add = (
    id: string,
    agentId: string | null | undefined,
    title: string,
    body: string,
    itemId?: string,
    at?: number | null,
    sound = false,
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
      ...(sound ? { sound } : {}),
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
    if (request.method === SIGN_IN_METHOD) {
      const times = signInTimes(request);
      if (!times || now < times.alertAt) continue;
      add(
        `signin:${request.id}`,
        request.agent,
        "Sign-in needed",
        `Sign in at ${times.host}. Open the card in this chat.`,
        request.id,
        times.alertAt,
        true,
      );
      const expiresAt = (request.params as { expiresAt?: unknown } | undefined)
        ?.expiresAt;
      if (
        times.reminderAt !== null &&
        typeof expiresAt === "number" &&
        now >= times.reminderAt &&
        now <= expiresAt
      )
        add(
          `signin-reminder:${request.id}`,
          request.agent,
          "Sign-in card closes soon",
          `Sign in at ${times.host} within ${Math.round(SIGN_IN_REMINDER_SECONDS / 60)} minutes, or the card closes.`,
          request.id,
          times.reminderAt,
          true,
        );
      continue;
    }
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
