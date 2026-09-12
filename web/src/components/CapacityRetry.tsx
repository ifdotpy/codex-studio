import ErrorDescription from "./ErrorDescription";
import { useEffect, useRef, useState } from "react";
import { Button } from "@mantine/core";
import { api, errorText } from "../api";
import type { Json } from "../types";

export default function CapacityRetry({
  agentId,
  retry,
}: {
  agentId: string;
  retry: Json;
}) {
  const [confirmed, setConfirmed] = useState<Json | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [now, setNow] = useState(() => Date.now() / 1000);
  const lock = useRef(false);
  const current =
    confirmed &&
    confirmed.id === retry.id &&
    (confirmed.updatedAt || 0) > (retry.updatedAt || 0)
      ? confirmed
      : retry;
  const scheduled = current.status === "scheduled";
  useEffect(() => {
    if (!scheduled) return;
    setNow(Date.now() / 1000);
    const timer = setInterval(() => setNow(Date.now() / 1000), 250);
    return () => clearInterval(timer);
  }, [scheduled, current.dueAt]);
  const seconds =
    typeof current.dueAt === "number"
      ? Math.max(0, Math.ceil(current.dueAt - now))
      : 0;
  const available =
    !current.claimedAt &&
    ["scheduled", "cancelled", "exhausted"].includes(current.status);
  const action = async (choice: "retry" | "cancel") => {
    if (lock.current) return;
    lock.current = true;
    setPending(true);
    setError("");
    try {
      const result = await api<Json>(
        "/api/capacity-retry",
        {
          id: agentId,
          retry_id: retry.id,
          action: choice,
        },
        { timeoutMs: 15000 },
      );
      setConfirmed(result);
    } catch (cause) {
      setError(
        choice === "cancel"
          ? `Cancel could not be confirmed. The automatic retry may still start. ${errorText(cause)}`
          : `Retry could not be confirmed. Check the chat status. ${errorText(cause)}`,
      );
    } finally {
      lock.current = false;
      setPending(false);
    }
  };
  return (
    <div className="capacity-retry" data-retry-status={current.status}>
      <p role="status">
        {scheduled
          ? seconds > 0
            ? `Automatic retry in ${seconds}s.`
            : "Waiting for the server to retry."
          : current.status === "cancelled"
            ? "Automatic retry cancelled."
            : current.status === "exhausted"
              ? "Automatic retries finished. You can retry now."
              : current.status === "unknown"
                ? "Waiting for Codex to confirm the retry."
                : current.status === "failed"
                  ? "The retry could not start."
                  : current.acceptedTurnId
                    ? "The model retry started."
                    : "Starting the model retry."}
      </p>
      {current.reason && (
        <p>
          <ErrorDescription value={current.reason} role="status" />
        </p>
      )}
      {available && (
        <div className="native-error-actions">
          <Button
            size="compact-xs"
            disabled={pending}
            onClick={() => void action("retry")}
          >
            Retry now
          </Button>
          {scheduled && (
            <Button
              size="compact-xs"
              variant="subtle"
              disabled={pending}
              onClick={() => void action("cancel")}
            >
              Cancel automatic retry
            </Button>
          )}
        </div>
      )}
      {error && <p role="alert">{error}</p>}
    </div>
  );
}
