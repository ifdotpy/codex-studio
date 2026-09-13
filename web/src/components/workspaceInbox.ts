import type { Json, Snapshot } from "../types";

// Questions and user tasks use the chat snapshot. Work review records can
// arrive independently without delaying those items.
export function workspaceInbox(
  data: Snapshot,
  work: Json[] = data.runtime.work || [],
): Json[] {
  const ids = new Set(data.threads.map((agent) => agent.id));
  const runtime = data.runtime;
  return [
    ...(runtime.userTasks || [])
      .filter((task) => ids.has(task.agent) && task.status === "open")
      .map((task) => ({
        id: task.id,
        kind: "user_task",
        agent: task.agent,
        title: task.title,
        text: task.reason || task.criteria,
      })),
    ...work
      .filter((task) => ids.has(task.rootId) && task.status === "review")
      .map((task) => ({
        id: task.id,
        kind: "work",
        agent: task.rootId,
        title: task.title,
        text: "Result awaits acceptance",
      })),
    ...data.threads
      .filter((agent) => ["failed", "interrupted"].includes(agent.status))
      .map((agent) => ({
        id: agent.id,
        kind: "agent",
        agent: agent.id,
        title: agent.name,
        text: agent.error || agent.status,
      })),
    ...(runtime.monitors || [])
      .filter(
        (monitor) =>
          ids.has(monitor.agent) &&
          ["failed", "lost"].includes(monitor.status) &&
          !monitor.ruleId,
      )
      .map((monitor) => ({
        id: monitor.id,
        kind: "monitor",
        agent: monitor.agent,
        title: monitor.command,
        text: monitor.error || `Exit ${monitor.exitCode}`,
      })),
    ...(runtime.rules || [])
      .filter((rule) => ids.has(rule.agent) && rule.error)
      .map((rule) => ({
        id: rule.id,
        kind: "rule",
        agent: rule.agent,
        title: rule.name,
        text: rule.error,
      })),
  ];
}
