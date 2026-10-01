import type { BackgroundTask, Snapshot } from "../types";

export const activeTask = (task: BackgroundTask) =>
  ["running", "starting", "approval", "pending", "stopping"].includes(
    task.status,
  );

export function backgroundTasks(data: Snapshot | null): BackgroundTask[] {
  return [
    ...(data?.runtime.monitors || []).map(
      (monitor) => ({ ...monitor, kind: "monitor" as const }) as BackgroundTask,
    ),
    ...(data?.runtime.tasks || []),
  ];
}
