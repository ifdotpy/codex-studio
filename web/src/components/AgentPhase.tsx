import {
  Brain,
  CircleCheck,
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
  const phase = active ? agent.activity?.phase || agent.status : agent.status;
  const names: Record<string, string> = {
    thinking: "Thinking",
    writing: "Writing",
    tool: "Using tools",
    running: "Working",
    starting: "Starting",
    queued: "Queued",
    waiting: "Waiting for agents or commands",
    approval: "Waiting for your answer",
    completed: "Complete",
    failed: "Failed",
    paused: "Stopped",
    interrupted: "Interrupted",
    idle: "Ready",
  };
  const Icon =
    phase === "thinking"
      ? Brain
      : phase === "writing"
        ? PenLine
        : phase === "tool"
          ? Wrench
          : phase === "completed"
            ? CircleCheck
            : LoaderCircle;
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
