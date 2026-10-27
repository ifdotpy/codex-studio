import type { Snapshot } from "../types";
import type { BackgroundTask, Monitor, WorkspaceTask } from "../types";
import type { components } from "../generated/api";

type Task = BackgroundTask | WorkspaceTask;
export type RendererTaskKind =
  | components["schemas"]["TaskKind"]
  | "backgroundTask";
type DisplayTask<T extends Task | Monitor> = Omit<
  T,
  "agent" | "created" | "kind" | "status"
> &
  Pick<BackgroundTask, "tail"> & {
    [Field in "agent" | "created" | "status"]: NonNullable<T[Field]>;
  } & {
    kind: RendererTaskKind;
  };
export type DisplayLegacyTask =
  | DisplayTask<BackgroundTask>
  | DisplayTask<WorkspaceTask>
  | DisplayTask<Monitor>;
export type DisplayProcessTask =
  components["schemas"]["ManagedProcessRecord"] & {
    kind: RendererTaskKind;
    tail: string;
    interactive: components["schemas"]["ProcessCapabilities"]["sendInput"];
  };
export type DisplayBackgroundTask = DisplayLegacyTask | DisplayProcessTask;

function processTaskKind(
  source: DisplayProcessTask["source"],
): RendererTaskKind {
  switch (source) {
    case "monitor":
      return "monitor";
    case "codex_task":
      return "command";
    case "claude_task":
      return "backgroundTask";
    default:
      return source satisfies never;
  }
}

export function displayProcess(
  process: components["schemas"]["ManagedProcessRecord"],
): DisplayProcessTask {
  return {
    ...process,
    kind: processTaskKind(process.source),
    tail: process.output || "",
    interactive: process.capabilities.sendInput,
  };
}

export function processIdentity(
  task: DisplayProcessTask,
): components["schemas"]["ProcessIdentity"] {
  return {
    key: task.key,
    generation: task.generation,
    agent: task.agent,
    accountKey: task.accountKey,
    epoch: task.epoch,
  };
}

export const activeTask = (task: Pick<DisplayBackgroundTask, "status">) =>
  ["running", "starting", "approval", "pending", "stopping"].includes(
    task.status,
  );

export function isCommandTask(task: Pick<BackgroundTask, "kind" | "command">) {
  return task.kind === "command" || (task.kind == null && !!task.command);
}

export function projectTaskForRenderer<T extends Task | Monitor>(
  task: T,
  source?: "task" | "monitor",
): DisplayTask<T> | null;
export function projectTaskForRenderer(
  task: Task | Monitor,
  source: "task" | "monitor" = "task",
): DisplayLegacyTask | null {
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
  const rendererKind: RendererTaskKind =
    source === "task" && claudeTask ? "backgroundTask" : kind;
  const row = {
    ...task,
    agent: task.agent,
    created: task.created,
    accountKey:
      "accountKey" in task && typeof task.accountKey === "string"
        ? task.accountKey
        : undefined,
    kind: rendererKind,
    status: task.status,
  };
  return row;
}

export function backgroundTasks(
  data: Snapshot | null,
): (DisplayTask<BackgroundTask> | DisplayTask<Monitor>)[] {
  if (!data) return [];
  return [
    ...data.runtime.monitors
      .map((monitor) => projectTaskForRenderer(monitor, "monitor"))
      .filter((task): task is DisplayLegacyTask => task !== null),
    ...data.runtime.tasks
      .map((task) => projectTaskForRenderer(task, "task"))
      .filter(
        (task): task is DisplayLegacyTask =>
          task?.kind === "command" || task?.kind === "backgroundTask",
      ),
  ];
}
