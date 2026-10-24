import {
  Brain,
  CheckCheck,
  Circle,
  CircleAlert,
  CircleHelp,
  ClipboardList,
  Crown,
  LoaderCircle,
  Pause,
  PenLine,
  ScanEye,
  Terminal,
  Timer,
  Wrench,
} from "lucide-react";
import type { Agent } from "../../types";
import type { ChatIndicatorKind } from "../chat-status/chatStatusModel";

const stateIcons = {
  answer: CircleHelp,
  error: CircleAlert,
  working: LoaderCircle,
  unread: CheckCheck,
  paused: Pause,
  none: Circle,
};
export function TeamStateIcon({ kind }: { kind: ChatIndicatorKind }) {
  const Icon = stateIcons[kind];
  return (
    <Icon
      className={`team-state-icon team-state-icon-${kind}`}
      aria-hidden="true"
    />
  );
}
export function TeamRoleIcon({ agent, lead }: { agent: Agent; lead: boolean }) {
  const role =
    lead || agent.isLead || ["lead", "orchestrator"].includes(agent.role || "")
      ? "Lead"
      : agent.role === "reviewer"
        ? "Reviewer"
        : "Worker";
  const Icon = role === "Lead" ? Crown : role === "Reviewer" ? ScanEye : Wrench;
  return (
    <span className="worker-role" role="img" aria-label={role} title={role}>
      <Icon aria-hidden="true" />
    </span>
  );
}
export function activityPresentation(agent: Agent) {
  if (agent.commandProgress?.length)
    return { Icon: Terminal, label: "Command" };
  if (agent.status === "waiting_on_monitor")
    return { Icon: Timer, label: "Waiting on monitor" };
  if (["waiting", "parked", "queued", "starting"].includes(agent.status || ""))
    return { Icon: Timer, label: "Waiting" };
  if (agent.activity?.phase === "thinking")
    return { Icon: Brain, label: "Thinking" };
  if (agent.activity?.phase === "writing")
    return { Icon: PenLine, label: "Writing" };
  if (
    agent.activity?.tools?.some((tool) =>
      /command|exec/i.test(tool.type + tool.name),
    )
  )
    return { Icon: Terminal, label: "Command" };
  if (agent.activity?.phase === "tool")
    return { Icon: Wrench, label: "Using tools" };
  return { Icon: ClipboardList, label: "Task" };
}
