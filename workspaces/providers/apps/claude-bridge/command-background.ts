import type { ActiveQuery, BackgroundTask } from "./bridge.ts";
import type { BridgeParams } from "./bridge-protocol.ts";
import type {
  SDKTaskStartedMessage,
  SDKTaskUpdatedMessage,
  SDKTaskNotificationMessage,
} from "@anthropic-ai/claude-agent-sdk";
type TaskEvent =
  | Pick<SDKTaskStartedMessage, "subtype" | "is_backgrounded">
  | Pick<SDKTaskUpdatedMessage, "subtype" | "patch">
  | Pick<SDKTaskNotificationMessage, "subtype">;

// Move a running Claude Bash call to the background (the Ctrl+B control).
//
// Claude Code registers a foreground Bash call as a `local_bash` task about two
// seconds after it starts. Only a registered foreground task can move to the
// background, so the command item declares `canMoveToBackground` from those task
// events and Studio shows the control only while the flag is true.

const FOREGROUND_SHELL = "local_bash";

// Updates the command item for a task event. Returns the item to publish again,
// or null when the event does not change a command item.
export function foregroundCommandUpdate(
  active: Pick<ActiveQuery, "tools">,
  task: BackgroundTask,
  message: TaskEvent,
) {
  if (
    task.task_type !== FOREGROUND_SHELL ||
    !("tool_use_id" in task) ||
    !task.tool_use_id
  )
    return null;
  const item = active.tools.get(task.tool_use_id);
  if (item?.type !== "commandExecution" || item.status !== "inProgress")
    return null;
  let available = item.canMoveToBackground === true;
  if (message.subtype === "task_started")
    available = message.is_backgrounded === false;
  if ("patch" in message && message.patch.is_backgrounded === true)
    available = false;
  if (
    message.subtype === "task_notification" ||
    ("patch" in message &&
      ["completed", "failed", "killed"].some(
        (status) => status === message.patch.status,
      ))
  )
    available = false;
  if (available === (item.canMoveToBackground === true)) return null;
  item.canMoveToBackground = available;
  return item;
}

// Fields for a Bash result whose command keeps running as a background task.
// A backgrounded command has no exit code yet, so none is reported.
export function backgroundedCommandFields(toolUseResult: unknown) {
  const taskId =
    toolUseResult &&
    typeof toolUseResult === "object" &&
    "backgroundTaskId" in toolUseResult
      ? toolUseResult.backgroundTaskId
      : undefined;
  if (typeof taskId !== "string" || !taskId) return null;
  return {
    canMoveToBackground: false,
    backgroundTaskId: taskId,
    backgroundedByUser:
      toolUseResult !== null &&
      typeof toolUseResult === "object" &&
      "backgroundedByUser" in toolUseResult &&
      toolUseResult.backgroundedByUser === true,
  };
}

// Handles `thread/commandExecution/background`. The request names the exact
// turn and item. A repeated or late request cannot repeat the command: Claude
// answers false when the item has no foreground task.
export async function moveCommandToBackground(
  active:
    | (Pick<ActiveQuery, "turn" | "tools"> & {
        q?: Partial<
          Pick<NonNullable<ActiveQuery["q"]>, "backgroundTasks">
        > | null;
      })
    | undefined,
  params: Pick<
    BridgeParams<"thread/commandExecution/background">,
    "turnId" | "itemId"
  >,
) {
  if (!active?.turn || active.turn.id !== params.turnId)
    throw new Error("The command belongs to a different Claude turn");
  const item = active.tools.get(params.itemId);
  if (item?.type !== "commandExecution")
    throw new Error("Select a running command");
  if (item.status !== "inProgress" || item.backgroundTaskId)
    return { status: "not_running" };
  if (item.canMoveToBackground !== true)
    throw new Error("This command cannot move to the background yet");
  if (typeof active.q?.backgroundTasks !== "function")
    throw new Error(
      "This Claude Code version cannot move a command to the background",
    );
  const moved = await active.q.backgroundTasks(params.itemId);
  return { status: moved ? "backgrounded" : "not_running" };
}
