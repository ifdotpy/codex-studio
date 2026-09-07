import { useEffect, useMemo, useRef, useState } from "react";
import { errorText } from "../api";
import { panelContentDocument, type PanelCallback } from "./PanelDocument";
import "./agent-panel.css";

interface Panel {
  agent: string;
  version: number;
  dataVersion?: number;
  feed?: {
    monitorId: string;
    statePath: string;
    status: string;
    error?: string;
    updated?: number;
    sequence?: number;
  } | null;
  format?: string;
  spec?: unknown;
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
  dataVersion = 0,
  token,
}: {
  agentId: string;
  version: number;
  dataVersion?: number;
  token: string;
}) {
  const [panel, setPanel] = useState<Panel | null>(null);
  const [error, setError] = useState("");
  const [localError, setLocalError] = useState("");
  const [loading, setLoading] = useState(false);
  const [retry, setRetry] = useState(0);
  const [dismissedNotice, setDismissedNotice] = useState("");
  const [, redraw] = useState(0);
  const iframe = useRef<HTMLIFrameElement>(null);
  const [presentation, setPresentation] = useState({
    channel: "",
    // A nonzero area lets the browser paint and measure the hidden frame.
    height: 1,
    ready: false,
  });
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
          (next.dataVersion !== undefined &&
            (!Number.isSafeInteger(next.dataVersion) ||
              next.dataVersion < 0)) ||
          (next.feed != null &&
            (next.format !== "json-render" ||
              typeof next.feed !== "object" ||
              typeof next.feed.monitorId !== "string" ||
              typeof next.feed.status !== "string" ||
              typeof next.feed.statePath !== "string" ||
              !/^\/[A-Za-z][A-Za-z0-9_-]{0,63}$/.test(next.feed.statePath))) ||
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
  }, [agentId, version, dataVersion, retry]);
  useEffect(() => setLocalError(""), [agentId, version]);
  const current = version > 0 && panel?.agent === agentId ? panel : null;
  const callbacks = current?.callbacks || [];
  const key = JSON.stringify([agentId, current?.version || 0]);
  const state = actionState(key);
  // A data refresh must not recreate the iframe or reset local inputs and tabs.
  const template = useRef<Panel | null>(null);
  if (
    current?.agent !== template.current?.agent ||
    current?.version !== template.current?.version
  )
    template.current = current;
  const frameTemplate = template.current;
  const frameDocument = useMemo(() => {
    const channel = crypto.randomUUID();
    return {
      channel,
      html: panelContentDocument(
        frameTemplate || { html: "", css: "" },
        channel,
      ),
    };
  }, [frameTemplate, retry]);
  const enabled =
    !!current && current.version >= version && !error && !state.blocked;
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
    const panel = live.current;
    if (
      panel?.format === "json-render" &&
      panel.feed &&
      panel.spec &&
      panel.dataVersion
    ) {
      const value = (panel.spec as { state?: Record<string, unknown> }).state?.[
        panel.feed.statePath.slice(1)
      ];
      if (value !== undefined)
        iframe.current?.contentWindow?.postMessage(
          {
            type: "panel-data",
            channel: live.channel,
            statePath: panel.feed.statePath,
            value,
            dataVersion: panel.dataVersion,
          },
          "*",
        );
    }
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
      if (event.data?.type === "panel-error") {
        setError(String(event.data.error || "Cannot render panel"));
        return;
      }
      if (event.data?.type === "panel-local-error") {
        setLocalError(String(event.data.error || ""));
        return;
      }
      if (event.data?.type === "panel-size") {
        const height = event.data.height;
        if (
          typeof height === "number" &&
          Number.isFinite(height) &&
          height >= 0
        )
          setPresentation((old) => ({
            channel: live.channel,
            height: Math.min(150, height),
            ready: old.channel === live.channel && old.ready,
          }));
        return;
      }
      if (event.data?.type === "panel-ready") {
        setPresentation((old) => ({
          ...old,
          channel: live.channel,
          ready: true,
        }));
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
  const hasContent = !!(
    current?.spec ||
    current?.html.trim() ||
    current?.css.trim()
  );
  if (!version || (!hasContent && !error)) return null;
  const noticeKey = JSON.stringify([
    key,
    localError,
    feedback?.body.id,
    feedback?.status,
  ]);
  const noticeVisible =
    !error && (localError || feedback) && dismissedNotice !== noticeKey;
  const feedNoticeVisible =
    current?.feed &&
    (current.feed.error ||
      [
        "failed",
        "error",
        "stale",
        "stopped",
        "cancelled",
        "completed",
        "lost",
      ].includes(current.feed.status)) &&
    !noticeVisible &&
    !error;
  return (
    <>
      <section
        className={hasContent ? "agent-panel" : "agent-panel-notice"}
        style={
          hasContent
            ? {
                height: Math.max(
                  error ? 48 : 0,
                  noticeVisible || feedNoticeVisible ? 40 : 0,
                  presentation.height,
                ),
              }
            : undefined
        }
        data-ready={
          presentation.channel === frameDocument.channel && presentation.ready
        }
        aria-label="Agent panel"
        aria-busy={loading}
        data-agent={agentId}
        data-panel-version={current?.version || 0}
        data-panel-data-version={current?.dataVersion || 0}
      >
        {hasContent && (
          <iframe
            key={frameDocument.channel}
            ref={iframe}
            title="Agent panel content"
            sandbox="allow-scripts allow-forms"
            referrerPolicy="no-referrer"
            srcDoc={frameDocument.html}
          />
        )}
        {current?.feed && feedNoticeVisible && (
          <div
            className={`agent-panel-feedback ${current.feed.error || ["failed", "error", "stale", "lost"].includes(current.feed.status) ? "error" : ""}`}
            role="status"
            title={
              current.feed.error || "The last confirmed data remains visible."
            }
          >
            <span>
              {["stopped", "cancelled"].includes(current.feed.status)
                ? "Live data stopped"
                : current.feed.status === "completed"
                  ? "Live data ended"
                  : "Live data unavailable"}
            </span>
          </div>
        )}
        {noticeVisible && (
          <div
            className={`agent-panel-feedback ${localError ? "error" : feedback?.status}`}
            role={
              localError ||
              feedback?.status === "error" ||
              feedback?.status === "rejected"
                ? "alert"
                : "status"
            }
          >
            <span title={localError || feedback?.message}>
              {localError || feedback?.message}
            </span>
            {!localError && feedback?.status === "error" && enabled && (
              <button
                type="button"
                onClick={() => void deliver(state, feedback, token)}
              >
                Retry
              </button>
            )}
            {(localError || feedback?.status !== "error") && (
              <button
                type="button"
                aria-label="Dismiss panel notice"
                onClick={() => setDismissedNotice(noticeKey)}
              >
                ×
              </button>
            )}
          </div>
        )}
        {error && (
          <p className="agent-panel-error" role="alert">
            Cannot load the agent panel: {error}
            <button
              type="button"
              onClick={() => setRetry((value) => value + 1)}
            >
              Retry
            </button>
          </p>
        )}
      </section>
    </>
  );
}
