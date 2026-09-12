import { useEffect, useRef, useState } from "react";
import { ApiError, NetworkTimeoutError } from "../api";
import { onResume } from "../sync/resume";
import ErrorDescription from "./ErrorDescription";
import StreamingText from "./StreamingText";
import "./agent-panel.css";

interface ProgressState {
  scope: string;
  markdown: string;
  path: string;
  revision: string | null;
  error: unknown;
}

const pollIntervalMs = 1000;
const readTimeoutMs = 8000;

export default function AgentPanel({
  agentId,
  stateDir = "",
}: {
  agentId: string;
  stateDir?: string;
}) {
  const scope = JSON.stringify([stateDir, agentId]);
  const [state, setState] = useState<ProgressState | null>(null);
  const refresh = useRef<(() => void) | undefined>(undefined);

  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let request:
      | { controller: AbortController; started: number; expired: boolean }
      | undefined;
    let refreshPending = false;
    const visible = () => !document.hidden && navigator.onLine !== false;
    const empty = (): ProgressState => ({
      scope,
      markdown: "",
      path: "",
      revision: null,
      error: null,
    });
    const read = async () => {
      clearTimeout(timer);
      if (!active || !visible()) return;
      if (request) {
        // A suspended mobile browser can return before its deadline timer runs.
        if (Date.now() - request.started >= readTimeoutMs) {
          request.expired = true;
          request.controller.abort();
        }
        refreshPending = true;
        return;
      }
      const current = {
        controller: new AbortController(),
        started: Date.now(),
        expired: false,
      };
      request = current;
      const deadline = setTimeout(() => {
        current.expired = true;
        current.controller.abort();
      }, readTimeoutMs);
      try {
        const response = await fetch(
          `/api/panel?agent=${encodeURIComponent(agentId)}`,
          {
            signal: current.controller.signal,
            cache: "no-store",
          },
        );
        const next = await response.json();
        if (!response.ok || next?.error)
          throw new ApiError(
            next?.error || `Request failed (${response.status})`,
            response.status,
            next,
          );
        if (
          next?.agent !== agentId ||
          next.format !== "markdown" ||
          typeof next.markdown !== "string" ||
          typeof next.path !== "string" ||
          (next.revision !== null && typeof next.revision !== "string")
        )
          throw new Error("The PROGRESS.md response is invalid.");
        if (!active || current.controller.signal.aborted) return;
        setState((previous) => {
          if (
            previous?.scope === scope &&
            !previous.error &&
            previous.markdown === next.markdown &&
            previous.path === next.path &&
            previous.revision === next.revision
          )
            return previous;
          return {
            scope,
            markdown: next.markdown,
            path: next.path,
            revision: next.revision,
            error: null,
          };
        });
      } catch (error) {
        if (
          !active ||
          !visible() ||
          (current.controller.signal.aborted && !current.expired)
        )
          return;
        setState((previous) => ({
          ...(previous?.scope === scope ? previous : empty()),
          error: current.expired ? new NetworkTimeoutError() : error,
        }));
      } finally {
        clearTimeout(deadline);
        if (request === current) request = undefined;
        if (active && visible())
          timer = setTimeout(
            () => void read(),
            refreshPending ? 0 : pollIntervalMs,
          );
        refreshPending = false;
      }
    };
    const wake = () => {
      void read();
    };
    refresh.current = wake;
    const stopResume = onResume(wake);
    const visibility = () => {
      if (!document.hidden) return;
      clearTimeout(timer);
      request?.controller.abort();
    };
    document.addEventListener("visibilitychange", visibility);
    wake();
    return () => {
      active = false;
      clearTimeout(timer);
      request?.controller.abort();
      stopResume();
      document.removeEventListener("visibilitychange", visibility);
      if (refresh.current === wake) refresh.current = undefined;
    };
  }, [agentId, scope]);

  // A scope change hides the previous file before effect cleanup or a new read.
  const current = state?.scope === scope ? state : null;
  if (!current || (!current.markdown.trim() && !current.error)) return null;
  return (
    <section
      className="agent-panel"
      aria-label="Agent progress"
      data-agent={agentId}
      data-panel-revision={current.revision ?? undefined}
    >
      <div className="agent-panel-heading">
        <code title={current.path}>PROGRESS.md</code>
      </div>
      <div className="agent-panel-content">
        {!!current.markdown.trim() && (
          <StreamingText text={current.markdown} agentId={agentId} />
        )}
        {Boolean(current.error) && (
          <div className="agent-panel-error">
            <span>Cannot read PROGRESS.md. </span>
            <ErrorDescription value={current.error} />
            <button type="button" onClick={() => refresh.current?.()}>
              Retry
            </button>
          </div>
        )}
      </div>
    </section>
  );
}
