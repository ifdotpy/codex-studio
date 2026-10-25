import fs from "node:fs/promises";
import {
  QUIET_MIN_MS,
  PRESSURE_IDLE_MS,
  PRESSURE_RESTART_GUARD_MS,
} from "./shell-policy.generated.mjs";

export function backgroundOutputPath(result, text) {
  if (typeof result?.backgroundOutputPath === "string")
    return result.backgroundOutputPath;
  // Native Bash writes this suffix itself, after command output. Older SDKs
  // omit the structured path on local shells.
  const suffix = text.slice(text.lastIndexOf("Output is being written to: "));
  return /^Output is being written to: ([^\r\n]+?)(?:\.(?= (?:You will|If it|To check)|$)|$)/.exec(
    suffix,
  )?.[1];
}

export function replaceBackgroundTasks(previous, tasks) {
  return new Map(
    tasks.map((task) => [
      task.task_id,
      {
        ...(previous.get(task.task_id)?.run_id === task.run_id
          ? previous.get(task.task_id)
          : {}),
        ...task,
        // This snapshot contains background tasks only.
        is_backgrounded: true,
      },
    ]),
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
}) {
  const backgroundShell = (task) =>
    task?.task_type === "local_bash" &&
    task.is_backgrounded === true &&
    !["completed", "failed", "killed", "stopped"].includes(task.status);
  const watches = new Map();
  const idleMinutes = PRESSURE_IDLE_MS / 60_000;
  let lastUse = lastUseAt,
    lastReap = null;
  const touch = () => {
    lastUse = now();
  };
  async function tick(active, { settings, eligible, critical }) {
    const quietMs = settings?.quietThresholdMs;
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
          t.task_type === "local_agent" &&
          !["completed", "failed", "killed", "stopped"].includes(t.status),
      );
    for (const task of shells) {
      let watch = watches.get(task.task_id);
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
        watches.set(task.task_id, watch);
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
      const current = active.tasks.get(task.task_id);
      if (
        busy() ||
        !eligible ||
        delegates() ||
        !backgroundShell(current) ||
        current.run_id !== task.run_id
      )
        continue;
      const at = now();
      const report = (kind, message) =>
        emit({
          id: `${task.task_id}:${task.run_id ?? watch.started}:${watch.episode}:${kind}`,
          taskId: task.task_id,
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
          `Critical memory pressure: Studio requested a stop for background shell ${task.task_id} after ${idleMinutes} idle minutes.`,
        );
        try {
          await stop(task.task_id);
          report(
            "cleanup_stopped",
            `Critical memory pressure: Studio stopped background shell ${task.task_id}.`,
          );
        } catch {
          report(
            "cleanup_unknown",
            `Critical memory pressure: the stop outcome for background shell ${task.task_id} is unknown. Inspect the task before another stop.`,
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
          `Background shell ${task.task_id} is still running without new output for ${Math.floor((at - watch.outputAt) / 1000)} seconds. This is a check-in, not completion. Read its output and inspect its processes. Stop it if it is stuck. Leave it running if it is a server or watcher.`,
        );
      }
    }
    return { watched: watches.size };
  }
  return { tick, touch };
}
