import type { Snapshot } from "../types";
import type { BackgroundTask, Monitor, WorkspaceTask } from "../types";

type Task = BackgroundTask | WorkspaceTask;
type RendererTaskFields = {
  agent: string;
  created: number;
  kind: string;
  status: string;
  processKey?: string | undefined;
  generation?: string | undefined;
  accountKey?: string | undefined;
  epoch?: number | undefined;
  source?: "monitor" | "codex_task" | "claude_task" | undefined;
  capabilities?: {
    readOutput: boolean;
    sendInput: boolean;
    resize: boolean;
    stop: boolean;
  };
  outputFile?: string | null | undefined;
  outputWakeBudget?: Monitor["outputWakeBudget"] | undefined;
  outputPatterns?: string[] | undefined;
  tail?: string | null | undefined;
  error?: string | null | undefined;
  stdinError?: string | null | undefined;
  stdinClosed?: boolean | null | undefined;
  stdinCloseRequested?: boolean | string | null | undefined;
  bytes?: number | null | undefined;
  outputTruncated?: boolean | null | undefined;
  log?: string | null | undefined;
  timeout_ms?: number | null | undefined;
  cancelRequested?: boolean | null | undefined;
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
  kind?: string | null | undefined;
  command?: string | null | undefined;
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
