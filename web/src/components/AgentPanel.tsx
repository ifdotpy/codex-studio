import { useEffect, useMemo, useState } from "react";
import { errorText } from "../api";
import { isolatedDocument } from "./RichPreview";
import "./agent-panel.css";

interface Panel {
  agent: string;
  version: number;
  html: string;
  css: string;
  updated: number | null;
}

export default function AgentPanel({
  agentId,
  version,
}: {
  agentId: string;
  version: number;
}) {
  const [panel, setPanel] = useState<Panel | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setError("");
    if (!version) {
      setPanel(null);
      setLoading(false);
      return () => controller.abort();
    }
    setLoading(true);
    void (async () => {
      try {
        const response = await fetch(
          `/api/panel?agent=${encodeURIComponent(agentId)}`,
          { signal: controller.signal },
        );
        const next = await response.json();
        if (!response.ok)
          throw new Error(next.error || `Request failed (${response.status})`);
        if (
          next.agent !== agentId ||
          !Number.isInteger(next.version) ||
          next.version < version ||
          typeof next.html !== "string" ||
          typeof next.css !== "string"
        )
          throw new Error("The panel response is invalid.");
        if (!controller.signal.aborted) setPanel(next);
      } catch (e) {
        if (!controller.signal.aborted) setError(errorText(e));
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    })();
    return () => controller.abort();
  }, [agentId, version, retry]);
  const current = panel?.agent === agentId ? panel : null;
  const document = useMemo(
    () =>
      isolatedDocument(current?.html || "", {
        css: current?.css || "",
        dark: true,
      }),
    [current?.html, current?.css],
  );
  return (
    <section
      className="agent-panel"
      aria-label="Agent panel"
      aria-busy={loading}
      data-agent={agentId}
      data-panel-version={current?.version || 0}
    >
      {current?.html || current?.css ? (
        <iframe
          title="Agent panel content"
          sandbox=""
          referrerPolicy="no-referrer"
          srcDoc={document}
        />
      ) : (
        <div className="agent-panel-empty">
          {loading ? "Loading agent panel…" : "Agent panel"}
          {!loading && <span>Progress and updates appear here.</span>}
        </div>
      )}
      {error && (
        <p className="agent-panel-error" role="alert">
          Cannot load the agent panel: {error}
          <button type="button" onClick={() => setRetry((value) => value + 1)}>
            Retry
          </button>
        </p>
      )}
    </section>
  );
}
