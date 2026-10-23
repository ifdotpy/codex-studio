import { Button } from "@mantine/core";
import { useEffect, useRef, useState } from "react";
import { errorText, type PostResult } from "../api";
import { useServerApi } from "../servers/ServerServicesContext";

type Usage = PostResult<"/api/claude/file-backups">;
export default function ClaudeFileBackups({
  agentId,
  onReleased,
}: {
  agentId: string;
  onReleased: () => void;
}) {
  const { post } = useServerApi();
  const [usage, setUsage] = useState<Usage | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const requestId = useRef<string | null>(null);
  useEffect(() => {
    let current = true;
    post("/api/claude/file-backups", {
      id: agentId,
      release: false,
      request_id: crypto.randomUUID(),
    })
      .then((value) => {
        if (current) setUsage(value);
      })
      .catch((error) => {
        if (current) setMessage(errorText(error));
      });
    return () => {
      current = false;
    };
  }, [agentId, post]);
  return (
    <div>
      {usage && (
        <p>
          Backup usage: {usage.count} of {usage.maxCount} copies, {usage.bytes}{" "}
          of {usage.maxBytes} bytes.
        </p>
      )}
      <p>
        Release deletes the saved original files from completed restores in this
        chat. Unresolved restore backups stay saved.
      </p>
      <Button
        disabled={busy || !usage}
        onClick={async () => {
          setBusy(true);
          requestId.current ||= crypto.randomUUID();
          try {
            const value = await post("/api/claude/file-backups", {
              id: agentId,
              release: true,
              request_id: requestId.current,
            });
            requestId.current = null;
            setUsage(value);
            setMessage(
              value.released
                ? `Released ${value.released} backups.`
                : "No completed backups can be released in this chat. Check other Claude chats.",
            );
            if (value.released) onReleased();
          } catch (error) {
            setMessage(errorText(error));
          } finally {
            setBusy(false);
          }
        }}
      >
        Release completed backups
      </Button>
      {message && <p role="status">{message}</p>}
    </div>
  );
}
