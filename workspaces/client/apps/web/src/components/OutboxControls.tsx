import { useServerOutbox } from "../servers/ServerServicesContext";
import { Button } from "@mantine/core";
import { useState } from "react";
import { errorText } from "../api";
import type { OutgoingMessage } from "../sync/send";

export default function OutboxControls({
  entry,
  onResolved,
}: {
  entry?: OutgoingMessage | undefined;
  edit: (entry: OutgoingMessage) => Promise<void>;
  onResolved?: (id: string) => void;
}) {
  const { changeOutbox } = useServerOutbox();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  if (
    !entry ||
    !["queued", "paused", "uncertain", "failed"].includes(entry.status)
  )
    return null;
  const run = async (action: "resume" | "cancel") => {
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      const result = await changeOutbox(entry.id, action);
      if (
        action === "cancel" ||
        (result && result.status !== "uncertain" && result.status !== "failed")
      )
        onResolved?.(entry.id);
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="outbox-controls">
      <div className="outbox-buttons">
        <Button
          size="compact-xs"
          disabled={busy}
          onClick={() => void run("resume")}
        >
          Retry
        </Button>
        <Button
          size="compact-xs"
          disabled={busy}
          onClick={() => void run("cancel")}
        >
          Discard
        </Button>
      </div>
      {error && <p role="alert">{error}</p>}
    </div>
  );
}
