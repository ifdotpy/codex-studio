import type { Snapshot } from "../types";
import type {
  BackgroundTask,
  DisplayBackgroundTask,
  Monitor,
  WorkspaceTask,
} from "../types";

type Task = BackgroundTask;

export const activeTask = (task: DisplayBackgroundTask) =>
  ["running", "starting", "approval", "pending", "stopping"].includes(
    task.status,
  );

export function projectTaskForRenderer(
  task: Task | WorkspaceTask | Monitor,
): DisplayBackgroundTask | null {
  const kind = "kind" in task ? task.kind : "monitor";
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
      .map(projectTaskForRenderer)
      .filter((task): task is DisplayBackgroundTask => task !== null),
    ...(data?.runtime.tasks ?? [])
      .map(projectTaskForRenderer)
      .filter((task): task is DisplayBackgroundTask => task !== null),
  ];
}
