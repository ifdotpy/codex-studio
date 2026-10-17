import type { Snapshot } from "../types";
import type { BackgroundTask, Monitor, WorkspaceTask } from "../types";

type Task = BackgroundTask | WorkspaceTask;
type RendererTaskFields = {
  agent: string;
  created: number;
  kind: string;
  status: string;
  processKey?: string;
  generation?: string;
  accountKey?: string;
  epoch?: number;
  source?: "monitor" | "codex_task" | "claude_task";
  capabilities?: {
    readOutput: boolean;
    sendInput: boolean;
    resize: boolean;
    stop: boolean;
  };
  tail?: string | null;
  error?: string | null;
  stdinError?: string | null;
  stdinClosed?: boolean | null;
  stdinCloseRequested?: boolean | string | null;
  bytes?: number | null;
  outputTruncated?: boolean | null;
  log?: string | null;
  timeout_ms?: number | null;
  cancelRequested?: boolean | null;
};
type DisplayTask<T> = Omit<T, keyof RendererTaskFields> & RendererTaskFields;
export type DisplayBackgroundTask =
  | DisplayTask<BackgroundTask>
  | DisplayTask<WorkspaceTask>
  | DisplayTask<Monitor>;

export const activeTask = (task: DisplayBackgroundTask) =>
  ["running", "starting", "approval", "pending", "stopping"].includes(
    task.status,
  );

export function isCommandTask(task: {
  kind?: string | null;
  command?: string | null;
}) {
  return task.kind === "command" || (task.kind == null && !!task.command);
}

export function projectTaskForRenderer(
  task: Task | Monitor,
  source: "task" | "monitor" = "task",
): DisplayBackgroundTask | null {
  const kind =
    source === "monitor" ? "monitor" : isCommandTask(task) ? "command" : "tool";
  const claudeTask =
    "kind" in task &&
    task.kind === "tool" &&
    "itemId" in task &&
    typeof task.itemId === "string" &&
    task.itemId.startsWith("task:");
  if (task.agent == null || task.created == null || task.status == null)
    return null;
  return {
    ...task,
    agent: task.agent,
    created: task.created,
    accountKey:
      "accountKey" in task && typeof task.accountKey === "string"
        ? task.accountKey
        : undefined,
    kind,
    status: task.status,
    ...(source === "task" && claudeTask ? { kind: "backgroundTask" } : {}),
  };
}

export function backgroundTasks(
  data: Snapshot | null,
): DisplayBackgroundTask[] {
  if (!data) return [];
  return [
    ...data.runtime.monitors
      .map((monitor) => projectTaskForRenderer(monitor, "monitor"))
      .filter((task): task is DisplayBackgroundTask => task !== null),
    ...data.runtime.tasks
      .map((task) => projectTaskForRenderer(task, "task"))
      .filter(
        (task): task is DisplayBackgroundTask =>
          task?.kind === "command" || task?.kind === "backgroundTask",
      ),
  ];
}
