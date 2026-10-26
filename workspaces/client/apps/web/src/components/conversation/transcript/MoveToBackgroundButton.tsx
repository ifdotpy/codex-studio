import { useState, type MouseEvent } from "react";
import { ArrowDownToLine, LoaderCircle } from "lucide-react";
import { useServerApi } from "../../../servers/ServerServicesContext";
import {
  checkCommandBackground,
  requestCommandBackground,
  type BackgroundOutcome,
} from "./commandBackground";
import "./command-background.css";

const MESSAGES: Partial<Record<BackgroundOutcome["kind"], string>> = {
  notRunning: "The command already ended or is in the background.",
};

/** Move one running command to the background. The agent continues at once. */
export default function MoveToBackgroundButton({
  agentId,
  turnId,
  itemId,
}: {
  agentId: string;
  turnId?: string | undefined;
  itemId: string;
}) {
  const api = useServerApi();
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<BackgroundOutcome>();
  const done = result?.kind === "backgrounded";
  const run = async (event: MouseEvent) => {
    // The button is inside a <summary>: do not open or close the card.
    event.preventDefault();
    event.stopPropagation();
    if (busy || done) return;
    setBusy(true);
    try {
      setResult(
        result?.kind === "uncertain"
          ? await checkCommandBackground(api, result.request)
          : await requestCommandBackground(api, agentId, turnId, itemId),
      );
    } finally {
      setBusy(false);
    }
  };
  const message =
    result && "message" in result
      ? result.message
      : result && MESSAGES[result.kind];
  return (
    <span className="tool-background" data-background-result={result?.kind}>
      {message && (
        <small className="tool-background-message" role="status">
          {message}
        </small>
      )}
      <button
        type="button"
        className="tool-background-button"
        onClick={run}
        disabled={busy || done}
        title="Keep the command running and let the agent continue. The agent gets the result when the command ends."
      >
        {busy ? (
          <LoaderCircle size={12} className="spin" />
        ) : (
          <ArrowDownToLine size={12} />
        )}
        {busy
          ? "Moving…"
          : done
            ? "Moved to background"
            : result?.kind === "uncertain"
              ? "Check result"
              : "Move to background"}
      </button>
    </span>
  );
}
