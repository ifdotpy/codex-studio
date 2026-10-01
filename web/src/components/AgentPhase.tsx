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
import { endedWaitLabel, type ChatWaitState } from "./chatStatusModel";
import "./provider-activity.css";
export default function AgentPhase({
  agent,
  connection,
  wait,
}: {
  agent?: Agent;
  connection: string;
  wait?: ChatWaitState;
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
  const currentPhase = nativeThreadError(agent)
    ? "blocked"
    : currentCapacityRetry(agent)?.status === "scheduled"
      ? "capacity-retry"
      : awaitingResponse
        ? "acknowledgement"
        : active
          ? agent.activity?.phase || agent.status
          : agent.status;
  const phase =
    wait?.live && ["idle", "completed"].includes(currentPhase)
      ? "waiting"
      : currentPhase;
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
    waiting: wait?.label || endedWaitLabel,
    parked: wait?.label || endedWaitLabel,
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
    waiting: Circle,
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
  const waiting = ["waiting", "parked"].includes(phase);
  const liveWait = waiting && wait?.live && connection !== "reconnecting";
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
      className={`agent-phase ${(active && !waiting) || liveWait ? "active" : ""}`}
      role="status"
      data-phase={phase}
      data-wait-state={waiting ? (wait?.live ? "live" : "ended") : undefined}
      data-agent-provider={agent.provider || "codex"}
    >
      {liveWait ? (
        <span className="phase-dots wait-dots" aria-hidden="true">
          <i />
          <i />
          <i />
        </span>
      ) : (
        <Icon size={15} />
      )}
      {waiting && wait?.live && connection !== "reconnecting" ? (
        <details className="agent-phase-wait">
          <summary>{wait.label}</summary>
          <div className="agent-phase-wait-details">
            <p>{wait.label}</p>
            <ul>
              {wait.agents.map((child) => (
                <li key={child.id}>Agent: {child.name}</li>
              ))}
              {wait.commands.map((command) => (
                <li key={command.id}>Command: {command.command}</li>
              ))}
              {wait.monitors.map((monitor) => (
                <li key={monitor.id}>
                  Monitor: {monitor.command || monitor.id}
                </li>
              ))}
              {wait.event && <li>Event: {wait.event}</li>}
              {wait.inputs > 0 && (
                <li>
                  Input: {wait.inputs}{" "}
                  {wait.inputs === 1 ? "answer" : "answers"} needed
                </li>
              )}
            </ul>
          </div>
        </details>
      ) : (
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
      )}
      {active && !waiting && connection !== "reconnecting" && (
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
