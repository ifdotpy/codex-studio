import { useEffect, useState } from "react";
import { busy, type Agent } from "../../types";
import { useVisualActivity } from "../../hooks/useVisualActivity";

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
          title={`The worker finished its turn without submitting task ${agent.stall.taskId}. Its lead decides the next step.`}
        >
          Waiting for lead · task {agent.stall.taskId} not submitted
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

/** Age of the current activity; terminal rows freeze at the last server update. */
export function AgentElapsed({ agent }: { agent: Agent }) {
  const [visualRef, visible] = useVisualActivity<HTMLSpanElement>();
  const [now, setNow] = useState(() => Date.now() / 1000);
  const active = busy.has(agent.status || "");
  const ticking = active && visible;
  useEffect(() => {
    if (!ticking) return;
    const timer = window.setInterval(
      () => setNow(Date.now() / 1000),
      PROGRESS_CLOCK_INTERVAL_MS,
    );
    return () => window.clearInterval(timer);
  }, [ticking]);
  const started =
    agent.commandProgress?.[0]?.startedAt ||
    agent.activity?.at ||
    agent.created;
  if (started == null) return null;
  return (
    <span
      ref={visualRef}
      className="worker-elapsed"
      aria-label="Elapsed time"
      title="Time since current activity started"
    >
      {progressElapsed(started, active ? now : agent.updated || now)}
    </span>
  );
}
