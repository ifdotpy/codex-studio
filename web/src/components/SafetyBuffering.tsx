import ErrorDescription from "./ErrorDescription";
import { useRef, useState } from "react";
import { Button, Modal } from "@mantine/core";
import { Clock3 } from "lucide-react";
import { api, errorText } from "../api";
import type { Agent } from "../types";

export default function SafetyBuffering({ agent }: { agent: Agent }) {
  const [confirm, setConfirm] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const [dismissed, setDismissed] = useState("");
  const lock = useRef(false);
  const b = agent.nativeSafetyBuffering;
  const stored = agent.nativeSafetyRetry;
  const retry =
    stored?.epoch === agent.epoch &&
    stored?.accountKey === (agent.accountKey || "default")
      ? stored
      : null;
  const active =
    !!retry && !["running", "failed", "cancelled"].includes(retry.stage);
  const visible =
    b?.showBufferingUi &&
    !b.responseStarted &&
    b.turnId === agent.turnId &&
    agent.inFlight &&
    !b.dismissed &&
    dismissed !== b.turnId;
  const failed =
    retry?.stage === "failed" &&
    (retry.turnId === agent.turnId || !agent.turnId);
  if (!visible && !active && !failed) return null;
  const choose = async (safety: "wait" | "retry" | "cancel") => {
    if (lock.current || !(b?.turnId || retry?.turnId)) return;
    lock.current = true;
    setPending(true);
    setError("");
    try {
      await api(
        "/api/action",
        {
          id: agent.id,
          action: { safety, turnId: b?.turnId || retry?.turnId },
        },
        { timeoutMs: 15000 },
      );
      setConfirm(false);
      if (safety === "wait") setDismissed(b.turnId);
    } catch (cause) {
      setError(errorText(cause));
    } finally {
      lock.current = false;
      setPending(false);
    }
  };
  return (
    <div
      className="native-safety"
      data-safety-state={active || failed ? retry.stage : "waiting"}
    >
      <div className="native-safety-title" role="status">
        <Clock3 size={16} />
        <strong>
          {active
            ? "Changing the model"
            : failed
              ? "The model change could not finish"
              : "Codex is checking this request"}
        </strong>
      </div>
      <p>
        {active ? (
          "Waiting for Codex to confirm. You can continue in another chat."
        ) : failed ? (
          <ErrorDescription value={retry.error} />
        ) : (
          "Extra safety checks can take longer. You can keep waiting."
        )}
      </p>
      {active && retry.error && (
        <p>
          <ErrorDescription value={retry.error} />
        </p>
      )}
      {active && retry.rpcMethod !== "turn/start" && (
        <Button
          size="compact-xs"
          variant="subtle"
          loading={pending}
          onClick={() => void choose("cancel")}
        >
          Cancel model change
        </Button>
      )}
      {visible && !active && !failed && (
        <div className="native-error-actions">
          {!!b.fasterModel && (
            <Button
              size="compact-xs"
              variant="light"
              onClick={() => setConfirm(true)}
            >
              Retry with a faster model
            </Button>
          )}
          <Button
            size="compact-xs"
            variant="subtle"
            loading={pending}
            onClick={() => void choose("wait")}
          >
            Keep waiting
          </Button>
          <a
            href="https://help.openai.com/en/articles/20001326"
            target="_blank"
            rel="noreferrer"
          >
            Learn more
          </a>
        </div>
      )}
      {error && <p role="alert">{error}</p>}
      <Modal
        opened={confirm && !!visible && !active}
        onClose={() => !pending && setConfirm(false)}
        title="Retry with a faster model"
        centered
      >
        <p>
          Codex offers {b?.fasterModel}. It may be less capable with complex
          requests.
        </p>
        <p>
          Studio will stop this turn and retry its input from the same prior
          context.
        </p>
        <div className="native-error-actions">
          <Button
            variant="subtle"
            disabled={pending}
            onClick={() => setConfirm(false)}
          >
            Keep waiting
          </Button>
          <Button loading={pending} onClick={() => void choose("retry")}>
            Stop and retry
          </Button>
        </div>
      </Modal>
    </div>
  );
}
