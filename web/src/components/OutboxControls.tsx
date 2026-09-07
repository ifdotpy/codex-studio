import { Button } from "@mantine/core";
import { useState } from "react";
import { errorText } from "../api";
import { changeOutbox, type OutgoingMessage } from "../sync/send";

export default function OutboxControls({
  entry,
  edit,
}: {
  entry?: OutgoingMessage;
  edit: (entry: OutgoingMessage) => Promise<void>;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  if (!entry || !["queued", "paused"].includes(entry.status)) return null;
  const run = async (action: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      await action();
    } catch (failure) {
      setError(errorText(failure));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="outbox-controls">
      <div className="outbox-buttons">
        {entry.attempted === false ? (
          <>
            <Button
              size="compact-xs"
              disabled={busy}
              onClick={() => void run(() => edit(entry))}
            >
              Edit message
            </Button>
            <Button
              size="compact-xs"
              disabled={busy}
              onClick={() => void run(() => changeOutbox(entry.id, "cancel"))}
            >
              Cancel send
            </Button>
          </>
        ) : (
          <Button
            size="compact-xs"
            disabled={busy}
            onClick={() =>
              void run(() =>
                changeOutbox(
                  entry.id,
                  entry.status === "paused" ? "resume" : "pause",
                ),
              )
            }
          >
            {entry.status === "paused" ? "Resume retries" : "Stop retries"}
          </Button>
        )}
      </div>
      {entry.attempted !== false && (
        <p>
          Delivery may have started. This button stops retries. It does not
          cancel work on the server.
        </p>
      )}
      {error && <p role="alert">{error}</p>}
    </div>
  );
}
