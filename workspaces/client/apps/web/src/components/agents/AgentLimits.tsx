import { useEffect, useState } from "react";
import { Button } from "@mantine/core";
import type { Agent } from "../../types";
import { errorText } from "../../api";
import { WORKER_TURN_LIMIT_REASON } from "../../generated/sharedValues";
import {
  useServerApi,
  useServerFileDialogs,
  useServerStorage,
} from "../../servers/ServerServicesContext";
import "./agent-limits.css";

export function WorkerLimits({ agent }: { agent: Agent }) {
  const { post, saved } = useServerApi();
  const storage = useServerStorage();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [continued, setContinued] = useState(false);
  const stopped = agent.terminalReason === WORKER_TURN_LIMIT_REASON;
  useEffect(() => {
    setContinued(false);
    setError("");
  }, [agent.id, agent.lastCompletedTurn, stopped]);
  if (agent.isLead) return null;
  const caps = agent.capabilityProfile;
  return (
    <section className="agent-limits" aria-label="Worker limits">
      <p>
        Turns used: {agent.completedWorkerTurns ?? 0} /{" "}
        {caps?.maxTurns ?? "Unlimited"}
      </p>
      {stopped && (
        <p role="status">
          <strong>Stopped: turn limit reached</strong>
        </p>
      )}
      {stopped && !agent.remoteWorker && !continued && (
        <>
          <p>Continue resets the turn count and resumes the assigned work.</p>
          <Button
            size="compact-sm"
            loading={busy}
            onClick={async () => {
              setBusy(true);
              setError("");
              try {
                const key = `worker-continue:${agent.id}:${agent.threadId || ""}:${agent.lastCompletedTurn || ""}`;
                const requestId =
                  saved<string | null>(key, null) || crypto.randomUUID();
                storage.setItem(key, JSON.stringify(requestId));
                await post("/api/agents/continue", {
                  id: agent.id,
                  request_id: requestId,
                });
                setContinued(true);
                // Keep the receipt identity if this card remounts before sync arrives.
              } catch (failure) {
                setError(errorText(failure));
              } finally {
                setBusy(false);
              }
            }}
          >
            Continue
          </Button>
        </>
      )}
      {continued && <p role="status">Continuation queued.</p>}
      {error && <p role="alert">{error}</p>}
      <details>
        <summary>Effective capabilities</summary>
        <dl>
          {(
            [
              ["allowedTools", "Allowed tools", "No allowed tools"],
              [
                "disallowedTools",
                "Disallowed tools",
                "No additional denied tools",
              ],
              ["mcpServers", "MCP servers", "No user MCP servers"],
              ["skills", "Skills", "No skill preload"],
            ] as const
          ).map(([key, label, empty]) => (
            <div key={key}>
              <dt>{label}</dt>
              <dd>
                {caps?.[key] == null
                  ? "Provider default"
                  : caps[key].length
                    ? caps[key].join(", ")
                    : empty}
              </dd>
            </div>
          ))}
        </dl>
        <p>
          These limits include the inherited profile. Provider permissions still
          apply.
        </p>
      </details>
    </section>
  );
}

export function RetainedWorktree({ agent }: { agent: Agent }) {
  const { canRevealPath, revealPath } = useServerFileDialogs();
  const [error, setError] = useState("");
  const cleanup = agent.workspaceCleanupIntent;
  const canOpen = !agent.remoteWorker && canRevealPath();
  if (!cleanup || !["not_sent", "unknown"].includes(cleanup.phase)) return null;
  return (
    <section className="agent-limits" aria-label="Retained worktree">
      <p>
        <strong>
          {cleanup.phase === "unknown"
            ? "Cleanup outcome unconfirmed"
            : agent.worktree || agent.workspaceMode === "worktree"
              ? "Worktree retained"
              : "Workspace retained"}
        </strong>
      </p>
      <p>
        {cleanup.reason ||
          "Cleanup did not confirm removal. Inspect the folder before another cleanup."}
      </p>
      {cleanup.path && <p className="agent-limits-path">{cleanup.path}</p>}
      <Button
        size="compact-sm"
        disabled={!cleanup.path || !canOpen}
        onClick={async () => {
          setError("");
          try {
            if (cleanup.path) await revealPath(cleanup.path);
          } catch (failure) {
            setError(errorText(failure));
          }
        }}
      >
        Open folder
      </Button>
      {!canOpen && <p>Open this path on the server that owns the worktree.</p>}
      {error && <p role="alert">{error}</p>}
    </section>
  );
}
