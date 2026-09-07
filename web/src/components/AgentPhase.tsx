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
export default function AgentPhase({
  agent,
  connection,
}: {
  agent?: Agent;
  connection: string;
}) {
  if (!agent) return null;
  const active = ["running", "starting"].includes(agent.status);
  const awaitingResponse =
    active &&
    (agent.startAttempt?.prepareError || agent.startAttempt?.responseError);
  const phase = awaitingResponse
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
    approval: Clock3,
    failed: CircleAlert,
    interrupted: CircleAlert,
    paused: Pause,
  };
  const Icon = connection === "reconnecting" ? WifiOff : icons[phase] || Circle;
  return (
    <div
      className={`agent-phase ${active ? "active" : ""}`}
      role="status"
      data-phase={phase}
    >
      <Icon size={15} />
      <span>
        {connection === "reconnecting"
          ? "Connection lost. Reconnecting…"
          : names[phase] || "Working"}
      </span>
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
