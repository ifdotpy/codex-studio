import type { Snapshot } from "../types";
import type { BackgroundTask, Monitor, WorkspaceTask } from "../types";

type Task = BackgroundTask | WorkspaceTask;
type RendererTaskFields = {
  agent: string;
  created: number;
  kind: string;
  status: string;
};
export type DisplayBackgroundTask = (Task | Monitor) & RendererTaskFields;

export const activeTask = (task: DisplayBackgroundTask) =>
  ["running", "starting", "approval", "pending", "stopping"].includes(
    task.status,
  );

export function projectTaskForRenderer(
  task: Task | Monitor,
  source: "task" | "monitor" = "task",
): DisplayBackgroundTask | null {
  const kind =
    source === "monitor" ? "monitor" : "kind" in task ? task.kind : null;
  if (
    task.agent == null ||
    task.created == null ||
    kind == null ||
    task.status == null
  )
    return null;
  return {
    ...task,
    agent: task.agent,
    created: task.created,
    kind,
    status: task.status,
  };
}

export function backgroundTasks(
  data: Snapshot | null,
): DisplayBackgroundTask[] {
  return [
    ...(data?.runtime.monitors ?? [])
      .map((monitor) => projectTaskForRenderer(monitor, "monitor"))
      .filter((task): task is DisplayBackgroundTask => task !== null),
    ...(data?.runtime.tasks ?? [])
      .map((task) => projectTaskForRenderer(task, "task"))
      .filter((task): task is DisplayBackgroundTask => task !== null),
  ];
}
