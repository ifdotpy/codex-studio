import { useEffect, useState } from "react";
import type { Agent } from "../../types";

/** Renderer clock refresh interval, in milliseconds, for managed-command elapsed time. */
const PROGRESS_CLOCK_INTERVAL_MS = 1000;

export function progressElapsed(startedAt: number, nowSeconds: number) {
  const seconds = Math.max(0, Math.floor(nowSeconds - startedAt));
  const minutes = Math.floor(seconds / 60);
  return minutes ? `${minutes}m ${seconds % 60}s` : `${seconds}s`;
}

export function commandWaitLabel(agent: Pick<Agent, "commandProgress">) {
  return `Waiting on ${agent.commandProgress?.[0]?.stage || "managed command"}`;
}

/** Visible in agent lists; uses the generated HTTP entity contract. */
export function AgentProgress({
  agent,
}: {
  agent: Pick<Agent, "commandProgress" | "stall">;
}) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  const active = Boolean(agent.commandProgress?.length);
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(
      () => setNow(Date.now() / 1000),
      PROGRESS_CLOCK_INTERVAL_MS,
    );
    return () => window.clearInterval(timer);
  }, [active]);
  return (
    <>
      {agent.stall && (
        <span
          className="agent-stall"
          role="status"
          title={`Task ${agent.stall.taskId} has not been submitted`}
        >
          Stalled · task {agent.stall.taskId} unsubmitted
        </span>
      )}
      {agent.commandProgress?.map((command) => (
        <span
          className="agent-command-progress"
          key={command.monitorId}
          data-monitor={command.monitorId}
        >
          <span className="agent-command-stage" title={command.stage}>
            {command.stage} ·{" "}
            <span aria-label="Elapsed time">
              {progressElapsed(command.startedAt, now)}
            </span>
          </span>
          {command.lastOutputLine && (
            <span
              className="agent-command-output"
              title={command.lastOutputLine}
            >
              {command.lastOutputLine}
            </span>
          )}
        </span>
      ))}
    </>
  );
}
