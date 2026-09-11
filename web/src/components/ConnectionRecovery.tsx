import { useEffect, useRef, useState } from "react";
import { Button } from "@mantine/core";
import { api, errorText } from "../api";
import type { Agent } from "../types";
import { nativeThreadError } from "../nativeErrors";

const disconnectErrors = new Set([
  "Codex disconnected. Review the transcript before resuming.",
  "Server restarted during a turn. Review history, then send a new instruction.",
]);

export function canCheckConnection(agent: Agent) {
  return (
    agent.source === "managed" &&
    agent.status === "interrupted" &&
    !agent.inFlight &&
    !agent.autoWake &&
    !agent.startAttempt &&
    !agent.accountTransferId &&
    !agent.workspaceOperation &&
    !agent.deletedAt &&
    !nativeThreadError(agent) &&
    !!agent.threadId &&
    !!agent.turnId &&
    typeof agent.error === "string" &&
    disconnectErrors.has(agent.error)
  );
}

export function matchingConnectionCheck(agent: Agent) {
  const receipt = agent.connectionCheck;
  return canCheckConnection(agent) &&
    receipt &&
    typeof receipt.at === "number" &&
    receipt.at > 0 &&
    (receipt.epoch ?? null) === (agent.epoch ?? null) &&
    (receipt.accountKey || "default") === (agent.accountKey || "default") &&
    receipt.threadId === agent.threadId &&
    receipt.turnId === agent.turnId &&
    receipt.previousError === agent.error
    ? receipt
    : null;
}

type RecoveryResult = {
  status: "reconciled" | "unconfirmed" | "superseded";
  outcome?: "completed" | "failed" | "interrupted";
  error?: string;
  checked?: boolean;
};

export default function ConnectionRecovery({
  agentId,
  refresh,
}: {
  agentId: string;
  refresh: () => Promise<void>;
}) {
  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState("");
  const lock = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const check = async () => {
    if (lock.current) return;
    lock.current = true;
    setPending(true);
    setMessage("");
    try {
      const result = await api<RecoveryResult>(
        "/api/connection-recovery",
        { id: agentId },
        { timeoutMs: 100000 },
      );
      if (!mounted.current) return;
      if (
        result.status === "reconciled" &&
        ["completed", "failed", "interrupted"].includes(result.outcome || "")
      ) {
        setMessage("Connection checked. Refreshing the conversation.");
        await refresh();
      } else if (result.status === "superseded") {
        setMessage(
          "The conversation changed while the connection was checked. Review its current state.",
        );
      } else {
        setMessage(
          result.error
            ? `Could not confirm the previous outcome. Review the history before continuing. ${result.error}`
            : "Connection checked. The previous outcome remains unconfirmed. Review the history before continuing.",
        );
        if (result.status === "unconfirmed" && result.checked === true)
          await refresh();
      }
    } catch (error) {
      if (mounted.current)
        setMessage(
          `Could not finish the connection check. ${errorText(error)}`,
        );
    } finally {
      lock.current = false;
      if (mounted.current) setPending(false);
    }
  };
  return (
    <div className="native-error-actions">
      <Button
        size="compact-xs"
        variant="light"
        loading={pending}
        disabled={pending}
        onClick={() => void check()}
      >
        Check connection
      </Button>
      {message && <p role="status">{message}</p>}
    </div>
  );
}
