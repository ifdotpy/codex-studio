import {
  Brain,
  Circle,
  Clock3,
  CircleAlert,
  Pause,
  WifiOff,
  LoaderCircle,
  PenLine,
  Wrench,
} from "lucide-react";
import type { Agent } from "../types";
import { currentCapacityRetry } from "../capacityRetry";
import { nativeThreadError } from "../nativeErrors";
import { displayError, errorDetails } from "../errorPresentation";
import ErrorDescription from "./ErrorDescription";
export default function AgentPhase({
  agent,
  connection,
}: {
  agent?: Agent;
  connection: string;
}) {
  if (!agent) return null;
  const safety = agent.nativeSafetyBuffering;
  if (
    safety?.showBufferingUi &&
    !safety.dismissed &&
    !safety.responseStarted &&
    safety.turnId === agent.turnId &&
    agent.inFlight
  )
    return null;
  // The terminal error appears in the transcript and the account notice.
  if (agent.status === "failed" && agent.error) return null;
  const active = ["running", "starting"].includes(agent.status);
  const awaitingResponse =
    active &&
    (agent.startAttempt?.prepareError || agent.startAttempt?.responseError);
  const phase = nativeThreadError(agent)
    ? "blocked"
    : currentCapacityRetry(agent)?.status === "scheduled"
      ? "capacity-retry"
      : awaitingResponse
        ? "acknowledgement"
        : active
          ? agent.activity?.phase || agent.status
          : agent.status;
  const names: Record<string, string> = {
    thinking: "Thinking",
    writing: "Writing",
    tool: "Using tools",
    running: "Working",
    starting: "Starting",
    acknowledgement: "Waiting for Codex",
    retrying: "Codex is retrying",
    "capacity-retry": "Waiting to retry model",
    blocked: "Chat stopped as a precaution",
    auth: "Restoring sign-in",
    safety: "Codex is checking this request",
    error: "Codex reported an error",
    queued: "Queued",
    waiting: "Waiting for agents or commands",
    approval: "Waiting for your answer",
    failed: "Failed",
    paused: "Stopped",
    interrupted: "Interrupted",
    idle: "Ready",
  };
  if (["idle", "completed"].includes(phase) && connection !== "reconnecting")
    return null;
  const icons: Record<string, typeof Brain> = {
    thinking: Brain,
    writing: PenLine,
    tool: Wrench,
    starting: LoaderCircle,
    running: LoaderCircle,
    queued: Clock3,
    waiting: Clock3,
    acknowledgement: Clock3,
    retrying: LoaderCircle,
    "capacity-retry": Clock3,
    auth: Clock3,
    safety: Clock3,
    error: CircleAlert,
    blocked: CircleAlert,
    approval: Clock3,
    failed: CircleAlert,
    interrupted: CircleAlert,
    paused: Pause,
  };
  const Icon = connection === "reconnecting" ? WifiOff : icons[phase] || Circle;
  const native = ["retrying", "auth", "safety"].includes(phase)
    ? agent.nativeStatus
    : null;
  const message =
    displayError(native?.message) ||
    displayError(native?.error?.message) ||
    names[phase] ||
    "Working";
  const details = errorDetails(native?.error?.additionalDetails);
  return (
    <div
      className={`agent-phase ${active ? "active" : ""}`}
      role="status"
      data-phase={phase}
    >
      <Icon size={15} />
      <div className="agent-phase-copy">
        <span>
          {connection === "reconnecting" ? (
            "Connection lost. Reconnecting…"
          ) : (
            <ErrorDescription
              value={native?.message || native?.error?.message || message}
              role="status"
            />
          )}
        </span>
        {connection === "reconnecting" && native && <p>{message}</p>}
        {typeof details === "string" && details && <pre>{details}</pre>}
      </div>
      {active && connection !== "reconnecting" && (
        <span className="phase-dots" aria-hidden="true">
          <i />
          <i />
          <i />
        </span>
      )}
      {phase === "tool" && agent.activity?.tools?.length > 1 && (
        <small>{agent.activity.tools.length} tools</small>
      )}
    </div>
  );
}
