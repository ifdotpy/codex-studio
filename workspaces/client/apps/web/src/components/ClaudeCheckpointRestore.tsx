import ClaudeFileBackups from "./ClaudeFileBackups";
import { Button, NativeSelect } from "@mantine/core";
import { useEffect, useState } from "react";
import { errorText, type PostResult } from "../api";
import type { Agent } from "../types";
import {
  useServerApi,
  useServerStorage,
} from "../servers/ServerServicesContext";

type State = Extract<PostResult<"/api/claude/session">, { turns: unknown[] }>;

export default function ClaudeCheckpointRestore({
  agent,
  revision,
  onRestored,
}: {
  agent: Agent;
  revision: number;
  onRestored: () => void;
}) {
  const { post, saved } = useServerApi();
  const storage = useServerStorage();
  const [state, setState] = useState<State | null>(null);
  const [turn, setTurn] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    let current = true;
    setState(null);
    setError("");
    post("/api/claude/session", { id: agent.id, action: "state" })
      .then((value) => {
        if (current && "turns" in value) setState(value);
      })
      .catch((failure) => current && setError(errorText(failure)));
    return () => {
      current = false;
    };
  }, [agent.id, revision, post]);
  const selected = state?.turns.find((item) => item.id === turn);
  const reason = busy
    ? "The restore is in progress."
    : agent.inFlight
      ? "Pause Claude before restoring files."
      : !state
        ? "The Claude session state is unavailable."
        : !selected
          ? "Select a message."
          : !selected.canRestoreFiles
            ? selected.restoreFilesUnavailableReason ||
              "This message has no SDK file checkpoint."
            : null;
  return (
    <>
      <p>
        Restore Claude file edits and the conversation to before a message. The
        agent stays paused.
      </p>
      <p className="workspace-muted">
        SDK file checkpoints cover Claude file tools. They do not cover shell
        commands or edits from other applications.
      </p>
      <NativeSelect
        label="Message to restore before"
        value={turn}
        onChange={(event) => setTurn(event.currentTarget.value)}
        data={[
          { value: "", label: "Select a message" },
          ...(state?.turns || []).map((item, index) => ({
            value: item.id,
            label: `${index + 1}. ${item.text}`,
          })),
        ]}
      />
      {reason && <p className="workspace-muted">{reason}</p>}
      {error && <p role="alert">{error}</p>}
      {error.includes("backup limit") && (
        <ClaudeFileBackups agentId={agent.id} onReleased={() => setError("")} />
      )}
      <Button
        color="orange"
        disabled={!!reason}
        loading={busy}
        onClick={async () => {
          setBusy(true);
          setError("");
          const key = `claude-file-restore:${agent.id}:${turn}`;
          const requestId =
            saved<string | null>(key, null) || crypto.randomUUID();
          storage.setItem(key, JSON.stringify(requestId));
          try {
            await post(
              "/api/claude/session",
              {
                id: agent.id,
                action: "rollback",
                turn_id: turn,
                request_id: requestId,
                restore_files: true,
              },
              { timeoutMs: 120000 },
            );
            storage.removeItem(key);
            setTurn("");
            setState(null);
            onRestored();
          } catch (failure) {
            setError(errorText(failure));
          } finally {
            setBusy(false);
          }
        }}
      >
        Restore files and conversation
      </Button>
    </>
  );
}
