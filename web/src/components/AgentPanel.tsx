import { useEffect, useMemo, useRef, useState } from "react";
import { errorText } from "../api";
import { panelDocument, type PanelCallback } from "./PanelDocument";
import "./agent-panel.css";

interface Panel {
  agent: string;
  version: number;
  html: string;
  css: string;
  callbacks?: PanelCallback[];
  submittedCallbacks?: string[];
  updated: number | null;
}
interface Attempt {
  body: {
    id: string;
    agent: string;
    version: number;
    callback: string;
    values: Record<string, string[]>;
  };
  label: string;
  status: "pending" | "accepted" | "error" | "rejected";
  message: string;
}
interface ActionState {
  attempts: Map<string, Attempt>;
  active?: Attempt;
  blocked?: boolean;
  latest?: Attempt;
}
// Keep the exact request after a lost response, including across chat switches.
// Remounts never send automatically; the user must press Retry.
const actions = new Map<string, ActionState>();
const listeners = new Set<() => void>();
const changed = () => listeners.forEach((listener) => listener());
const actionState = (key: string) => {
  let state = actions.get(key);
  if (!state) {
    state = { attempts: new Map() };
    actions.set(key, state);
  }
  return state;
};
async function deliver(state: ActionState, attempt: Attempt, token: string) {
  if (attempt.status === "pending") return;
  attempt.status = "pending";
  attempt.message = `Sending ${attempt.label}…`;
  state.active = state.latest = attempt;
  changed();
  try {
    const response = await fetch("/api/panel/callback", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Canvas-Token": token },
      body: JSON.stringify(attempt.body),
    });
    const result = await response.json();
    if (!response.ok) {
      if (response.status >= 400 && response.status < 500) {
        attempt.status = "rejected";
        attempt.message =
          result.error || "This panel action is no longer available.";
        state.blocked = true;
        state.active = undefined;
        return;
      }
      throw new Error(result.error || `Request failed (${response.status})`);
    }
    if (
      result.id !== attempt.body.id ||
      result.agent !== attempt.body.agent ||
      result.version !== attempt.body.version ||
      result.callback !== attempt.body.callback ||
      typeof result.status !== "string"
    )
      throw new Error(
        "The server response is invalid. Retry to check the same action.",
      );
    attempt.status = "accepted";
    attempt.message = `${attempt.label}: sent to agent`;
    state.active = undefined;
  } catch (error) {
    attempt.status = "error";
    attempt.message = `Delivery not confirmed: ${errorText(error)}`;
  } finally {
    changed();
  }
}

export default function AgentPanel({
  agentId,
  version,
  token,
}: {
  agentId: string;
  version: number;
  token: string;
}) {
  const [panel, setPanel] = useState<Panel | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [retry, setRetry] = useState(0);
  const [, redraw] = useState(0);
  const iframe = useRef<HTMLIFrameElement>(null);
  useEffect(() => {
    const listener = () => redraw((n) => n + 1);
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }, []);
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
          typeof next.css !== "string" ||
          (next.callbacks !== undefined && !Array.isArray(next.callbacks))
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
  const callbacks = current?.callbacks || [];
  const key = JSON.stringify([agentId, current?.version || 0]);
  const state = actionState(key);
  const frameDocument = useMemo(() => {
    const channel = crypto.randomUUID();
    return {
      channel,
      html: panelDocument(
        current?.html || "",
        current?.css || "",
        current?.callbacks || [],
        channel,
      ),
    };
  }, [current]);
  const enabled =
    !!current &&
    current.version >= version &&
    !loading &&
    !error &&
    !state.blocked;
  const latest = useRef({
    enabled,
    callbacks,
    state,
    current,
    token,
    channel: frameDocument.channel,
  });
  latest.current = {
    enabled,
    callbacks,
    state,
    current,
    token,
    channel: frameDocument.channel,
  };
  const sync = () => {
    const live = latest.current;
    iframe.current?.contentWindow?.postMessage(
      {
        type: "panel-state",
        channel: live.channel,
        busy: !live.enabled || !!live.state.active,
        locked: [
          ...live.state.attempts.keys(),
          ...(live.current?.submittedCallbacks || []),
        ],
      },
      "*",
    );
  };
  useEffect(() => {
    sync();
  });
  useEffect(() => {
    const receive = (event: MessageEvent) => {
      const live = latest.current;
      if (
        event.source !== iframe.current?.contentWindow ||
        event.data?.channel !== live.channel
      )
        return;
      if (event.data?.type === "panel-ready") {
        sync();
        return;
      }
      if (
        event.data?.type !== "panel-callback" ||
        !live.enabled ||
        !live.current ||
        live.state.active
      ) {
        sync();
        return;
      }
      const callback = live.callbacks.find(
        (item) => item.id === event.data.callback,
      );
      const values = event.data.values;
      if (
        !callback ||
        live.state.attempts.has(callback.id) ||
        live.current.submittedCallbacks?.includes(callback.id) ||
        !values ||
        typeof values !== "object" ||
        Array.isArray(values)
      ) {
        sync();
        return;
      }
      const fields = callback.fields || [];
      if (
        Object.keys(values).length > 32 ||
        new TextEncoder().encode(JSON.stringify(values)).length > 16384 ||
        Object.entries(values).some(
          ([name, value]) =>
            !fields.includes(name) ||
            !Array.isArray(value) ||
            value.length > 16 ||
            value.some(
              (entry) => typeof entry !== "string" || entry.length > 2000,
            ),
        )
      ) {
        setError("The form exceeds the panel action limits.");
        sync();
        return;
      }
      const attempt: Attempt = {
        body: {
          id: crypto.randomUUID(),
          agent: live.current.agent,
          version: live.current.version,
          callback: callback.id,
          values: JSON.parse(JSON.stringify(values)),
        },
        label: callback.label,
        status: "error",
        message: "",
      };
      live.state.attempts.set(callback.id, attempt);
      void deliver(live.state, attempt, live.token);
    };
    addEventListener("message", receive);
    return () => removeEventListener("message", receive);
  }, []);
  const feedback = state.latest;
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
          ref={iframe}
          title="Agent panel content"
          sandbox="allow-scripts allow-forms"
          referrerPolicy="no-referrer"
          srcDoc={frameDocument.html}
        />
      ) : (
        <div className="agent-panel-empty">
          {loading ? "Loading agent panel…" : "Agent panel"}
          {!loading && <span>Progress and updates appear here.</span>}
        </div>
      )}
      {error ? (
        <p className="agent-panel-error" role="alert">
          Cannot load the agent panel: {error}
          <button type="button" onClick={() => setRetry((value) => value + 1)}>
            Retry
          </button>
        </p>
      ) : (
        feedback && (
          <div
            className={`agent-panel-feedback ${feedback.status}`}
            role={
              feedback.status === "error" || feedback.status === "rejected"
                ? "alert"
                : "status"
            }
          >
            <span>{feedback.message}</span>
            {feedback.status === "error" && enabled && (
              <button
                type="button"
                onClick={() => void deliver(state, feedback, token)}
              >
                Retry
              </button>
            )}
          </div>
        )
      )}
    </section>
  );
}
