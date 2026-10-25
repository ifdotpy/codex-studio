import { useServerApi } from "../servers/ServerServicesContext";
import { useEffect, useRef, useState } from "react";
import { Button } from "@mantine/core";
import { errorText } from "../api";
import type { Agent } from "../types";
import { nativeThreadError } from "../nativeErrors";
import {
  BACKEND_RESTARTED_TURN_ERROR,
  LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
  PROVIDER_DISCONNECTED_TURN_ERROR,
  VM_PROVIDER_DISCONNECTED_TURN_ERROR,
} from "../generated/sharedValues";

// A provider connection ended. The server can check a start that has no turn
// ID yet in native history, so these errors do not need a turn ID.
const transportErrors = new Set<string>([
  PROVIDER_DISCONNECTED_TURN_ERROR,
  VM_PROVIDER_DISCONNECTED_TURN_ERROR,
  LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
]);
const disconnectErrors = new Set<string>([
  ...transportErrors,
  BACKEND_RESTARTED_TURN_ERROR,
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
    typeof agent.error === "string" &&
    disconnectErrors.has(agent.error) &&
    (!!agent.turnId || transportErrors.has(agent.error))
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
    (receipt.turnId ?? null) === (agent.turnId ?? null) &&
    receipt.previousError === agent.error
    ? receipt
    : null;
}

export default function ConnectionRecovery({
  agentId,
  refresh,
}: {
  agentId: string;
  refresh: () => Promise<void>;
}) {
  const { post } = useServerApi();

  const [pending, setPending] = useState(false);
  const [message, setMessage] = useState("");
  const [details, setDetails] = useState("");
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
    setDetails("");
    try {
      const result = await post(
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
        setDetails(result.error ? errorText(result.error) : "");
        setMessage(
          result.error
            ? "Could not confirm the previous outcome. Review the history before continuing."
            : "Connection checked. The previous outcome remains unconfirmed. Review the history before continuing.",
        );
        if (result.status === "unconfirmed" && result.checked === true)
          await refresh();
      }
    } catch (error) {
      if (mounted.current) {
        setMessage("Could not finish the connection check.");
        setDetails(errorText(error));
      }
    } finally {
      lock.current = false;
      if (mounted.current) setPending(false);
    }
  };
  return (
    <div className="native-error-actions studio-recovery-banner">
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
      {details && (
        <details>
          <summary>Details</summary>
          <p>{details}</p>
        </details>
      )}
    </div>
  );
}
