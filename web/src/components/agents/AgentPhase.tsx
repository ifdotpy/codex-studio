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
import type { Agent } from "../../types";
import { currentCapacityRetry } from "../../capacityRetry";
import { nativeThreadError } from "../../nativeErrors";
import { displayError, errorDetails } from "../../errorPresentation";
import ErrorDescription from "../ErrorDescription";
import {
  endedWaitLabel,
  type ChatWaitState,
} from "../chat-status/chatStatusModel";
import "./provider-activity.css";

function objectRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

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
  const safety = objectRecord(
    "nativeSafetyBuffering" in agent ? agent.nativeSafetyBuffering : undefined,
  );
  if (
    safety?.showBufferingUi === true &&
    safety.dismissed !== true &&
    safety.responseStarted !== true &&
    safety.turnId === agent.turnId &&
    agent.inFlight === true
  )
    return null;
  // The terminal error appears in the transcript and the account notice.
  if (agent.status === "failed" && agent.error) return null;
  const status = agent.status || "idle";
  const active = ["running", "starting"].includes(status);
  const awaitingResponse =
    active &&
    (agent.startAttempt?.prepareError || agent.startAttempt?.responseError);
  const currentPhase = nativeThreadError(agent)
    ? "blocked"
    : currentCapacityRetry(agent)?.status === "scheduled"
      ? "capacity-retry"
      : agent.status === "starting" && agent.worktreePreparation
        ? agent.worktreePreparation === "waiting"
          ? "folder-wait"
          : "folder-prepare"
        : awaitingResponse
          ? "acknowledgement"
          : active
            ? agent.activity?.phase || status
            : status;
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
    "folder-wait": "Waiting to prepare folder",
    "folder-prepare": "Preparing folder",
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
    "folder-wait": Clock3,
    "folder-prepare": LoaderCircle,
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
    ? objectRecord(agent.nativeStatus)
    : null;
  const nativeError = objectRecord(native?.error);
  const nativeMessage =
    typeof native?.message === "string" ? native.message : undefined;
  const nativeErrorMessage =
    typeof nativeError?.message === "string" ? nativeError.message : undefined;
  const message =
    displayError(nativeMessage) ||
    displayError(nativeErrorMessage) ||
    names[phase] ||
    "Working";
  const details = errorDetails(nativeError?.additionalDetails);
  return (
    <div
      className={`agent-phase ${(active && !waiting) || liveWait ? "active" : ""}${connection === "reconnecting" ? " studio-recovery-banner" : ""}`}
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
                value={nativeMessage || nativeErrorMessage || message}
                role="status"
              />
            )}
          </span>
          {(connection === "reconnecting" ||
            (typeof details === "string" && details)) && (
            <details>
              <summary>Details</summary>
              {connection === "reconnecting" && <p>{message}</p>}
              {typeof details === "string" && details && <pre>{details}</pre>}
            </details>
          )}
        </div>
      )}
      {active && !waiting && connection !== "reconnecting" && (
        <span className="phase-dots" aria-hidden="true">
          <i />
          <i />
          <i />
        </span>
      )}
      {phase === "tool" && (agent.activity?.tools?.length ?? 0) > 1 && (
        <small>{agent.activity?.tools?.length} tools</small>
      )}
    </div>
  );
}
