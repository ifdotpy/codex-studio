import fs from "node:fs/promises";
import type { BridgeTask } from "./bridge-protocol.ts";
import type { BackgroundShellPolicySettings } from "./runtime-contract.generated.ts";
import {
  QUIET_MIN_MS,
  PRESSURE_IDLE_MS,
  PRESSURE_RESTART_GUARD_MS,
} from "./shell-policy.generated.ts";
import { isRecord } from "./failure-causes.ts";

export type ShellPolicyReport = {
  id: string;
  taskId: string;
  kind: "quiet" | "cleanup_requested" | "cleanup_stopped" | "cleanup_unknown";
  message: string;
  at: number;
  outputFile: string | undefined;
};

export type ShellPolicyTick = {
  settings: BackgroundShellPolicySettings;
  eligible: boolean;
  critical: boolean;
};

type Watch = {
  run: string | undefined;
  started: number;
  outputAt: number;
  size: number | null;
  episode: number;
  checked: boolean;
  reaped: boolean;
};

const FINISHED = ["completed", "failed", "killed", "stopped"];

export function backgroundOutputPath(
  result: unknown,
  text: string,
): string | undefined {
  if (isRecord(result) && typeof result.backgroundOutputPath === "string")
    return result.backgroundOutputPath;
  // Native Bash writes this suffix itself, after command output. Older SDKs
  // omit the structured path on local shells.
  const suffix = text.slice(text.lastIndexOf("Output is being written to: "));
  return /^Output is being written to: ([^\r\n]+?)(?:\.(?= (?:You will|If it|To check)|$)|$)/.exec(
    suffix,
  )?.[1];
}

export function replaceBackgroundTasks(
  previous: Map<string, BridgeTask>,
  tasks: BridgeTask[],
): Map<string, BridgeTask> {
  return new Map(
    tasks.flatMap((task) => {
      if (!task.task_id) return [];
      const kept = previous.get(task.task_id);
      return [
        [
          task.task_id,
          {
            ...(kept?.run_id === task.run_id ? kept : {}),
            ...task,
            // This snapshot contains background tasks only.
            is_backgrounded: true,
          },
        ],
      ];
    }),
  );
}

// The bridge owns native task identity and output clocks. The server supplies
// its current durable eligibility and the host's memory-pressure verdict.
export function createShellPolicy({
  now = Date.now,
  stat = fs.stat,
  emit,
  stop,
  busy,
  pressureDisabled = !!process.env.CLAUDE_CODE_DISABLE_BG_SHELL_PRESSURE_REAP,
  lastUseAt = now(),
}: {
  now?: () => number;
  stat?: (file: string) => Promise<{ size: number }>;
  emit: (report: ShellPolicyReport) => void;
  stop: (taskId: string) => Promise<unknown>;
  busy: () => boolean;
  pressureDisabled?: boolean;
  lastUseAt?: number;
}) {
  const backgroundShell = (task: BridgeTask | undefined): task is BridgeTask =>
    task?.task_type === "local_bash" &&
    task.is_backgrounded === true &&
    !FINISHED.includes(task.status ?? "");
  const watches = new Map<string, Watch>();
  const idleMinutes = PRESSURE_IDLE_MS / 60_000;
  let lastUse = lastUseAt;
  let lastReap: number | null = null;
  const touch = () => {
    lastUse = now();
  };
  async function tick(
    active: { tasks: Map<string, BridgeTask> },
    { settings, eligible, critical }: ShellPolicyTick,
  ) {
    const quietMs = settings.quietThresholdMs;
    if (
      !(
        quietMs === 0 ||
        (Number.isSafeInteger(quietMs) && quietMs >= QUIET_MIN_MS)
      )
    )
      throw new Error("Invalid shellPolicy.quietThresholdMs");
    const shells = [...active.tasks.values()].filter(backgroundShell);
    const ids = new Set(shells.map((t) => t.task_id));
    for (const id of watches.keys()) if (!ids.has(id)) watches.delete(id);
    // Native delegates block all shell policy actions, as in Claude's host.
    const delegates = () =>
      [...active.tasks.values()].some(
        (t) =>
          t.task_type === "local_agent" && !FINISHED.includes(t.status ?? ""),
      );
    for (const task of shells) {
      const taskId = task.task_id;
      if (!taskId) continue;
      let watch = watches.get(taskId);
      if (!watch || watch.run !== task.run_id) {
        watch = {
          run: task.run_id,
          started: now(),
          outputAt: now(),
          size: null,
          episode: 0,
          checked: false,
          reaped: false,
        };
        watches.set(taskId, watch);
      }
      let measured = false;
      if (typeof task.output_file === "string") {
        try {
          const { size } = await stat(task.output_file);
          measured = true;
          if (watch.size !== null && size !== watch.size) {
            watch.outputAt = now();
            watch.episode++;
            watch.checked = false;
          }
          watch.size = size;
        } catch {
          /* An unreadable output file does not prove silence. */
          watch.outputAt = now();
        }
      }
      const current = active.tasks.get(taskId);
      if (
        busy() ||
        !eligible ||
        delegates() ||
        !backgroundShell(current) ||
        current.run_id !== task.run_id
      )
        continue;
      const at = now();
      const report = (kind: ShellPolicyReport["kind"], message: string): void =>
        emit({
          id: `${taskId}:${task.run_id ?? watch.started}:${watch.episode}:${kind}`,
          taskId,
          kind,
          message,
          at,
          outputFile: task.output_file,
        });
      const restartGuard =
        lastReap !== null &&
        lastReap > lastUse &&
        watch.started > lastReap &&
        watch.started - lastReap < PRESSURE_RESTART_GUARD_MS;
      if (
        critical === true &&
        settings.pressureCleanup === true &&
        !pressureDisabled &&
        at - lastUse >= PRESSURE_IDLE_MS &&
        !restartGuard &&
        !watch.reaped
      ) {
        // Latch before I/O. An unknown stop response never authorizes a repeat.
        watch.reaped = true;
        lastReap = at;
        report(
          "cleanup_requested",
          `Critical memory pressure: Studio requested a stop for background shell ${taskId} after ${idleMinutes} idle minutes.`,
        );
        try {
          await stop(taskId);
          report(
            "cleanup_stopped",
            `Critical memory pressure: Studio stopped background shell ${taskId}.`,
          );
        } catch {
          report(
            "cleanup_unknown",
            `Critical memory pressure: the stop outcome for background shell ${taskId} is unknown. Inspect the task before another stop.`,
          );
        }
      } else if (
        quietMs > 0 &&
        measured &&
        !watch.checked &&
        !watch.reaped &&
        at - watch.outputAt >= quietMs &&
        at - lastUse >= quietMs
      ) {
        watch.checked = true;
        report(
          "quiet",
          `Background shell ${taskId} is still running without new output for ${Math.floor((at - watch.outputAt) / 1000)} seconds. This is a check-in, not completion. Read its output and inspect its processes. Stop it if it is stuck. Leave it running if it is a server or watcher.`,
        );
      }
    }
    return { watched: watches.size };
  }
  return { tick, touch };
}
