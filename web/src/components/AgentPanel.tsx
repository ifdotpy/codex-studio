import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { NetworkTimeoutError } from "../api";
import { onResume } from "../sync/resume";
import ErrorDescription from "./ErrorDescription";
import FilePreview, { type PreviewTarget } from "./FilePreview";
import PreviewModal from "./PreviewModal";
import { localFileLink } from "./fileLinks";
import { relativeToDocument } from "./filePreviewFormats";
import { progressMarkdown } from "./ProgressMarkdown";
import { useProgressLayoutReport, type ProgressLayout } from "./progressLayout";
import {
  peekProgress,
  progressScope,
  readProgress,
  type CachedProgress,
} from "./progressCache";
import "./agent-panel.css";

interface ProgressState extends CachedProgress {
  cached: boolean;
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
  const scope = progressScope(stateDir, agentId);
  const [state, setState] = useState<ProgressState | null>(() => {
    const cached = peekProgress(stateDir, agentId);
    return cached ? { ...cached, cached: true, error: null } : null;
  });
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
      cached: true,
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
        const next = await readProgress(
          stateDir,
          agentId,
          current.controller.signal,
        );
        if (!active || current.controller.signal.aborted) return;
        setState((previous) => {
          if (
            previous?.scope === scope &&
            !previous.error &&
            !previous.cached &&
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
            cached: false,
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
          ...(previous?.scope === scope
            ? previous
            : peekProgress(stateDir, agentId) || empty()),
          error: current.expired ? new NetworkTimeoutError() : error,
          cached: true,
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
      if (!document.hidden && navigator.onLine !== false) return;
      clearTimeout(timer);
      request?.controller.abort();
      setState((previous) => {
        const cached =
          previous?.scope === scope
            ? previous
            : peekProgress(stateDir, agentId);
        return cached
          ? {
              ...cached,
              cached: true,
              error: "error" in cached ? cached.error : null,
            }
          : previous;
      });
    };
    document.addEventListener("visibilitychange", visibility);
    window.addEventListener("offline", visibility);
    wake();
    return () => {
      active = false;
      clearTimeout(timer);
      request?.controller.abort();
      stopResume();
      document.removeEventListener("visibilitychange", visibility);
      window.removeEventListener("offline", visibility);
      if (refresh.current === wake) refresh.current = undefined;
    };
  }, [agentId, scope, stateDir]);

  // A scope change hides the previous file before effect cleanup or a new read.
  const restored =
    state?.scope === scope ? null : peekProgress(stateDir, agentId);
  const current =
    state?.scope === scope
      ? state
      : restored
        ? { ...restored, cached: true, error: null }
        : null;
  if (!current || (!current.markdown.trim() && !current.error)) return null;
  return (
    <ProgressDisplay
      key={scope}
      current={current}
      agentId={agentId}
      scope={scope}
      retry={() => refresh.current?.()}
    />
  );
}

function ProgressDisplay({
  current,
  agentId,
  scope,
  retry,
}: {
  current: ProgressState;
  agentId: string;
  scope: string;
  retry: () => void;
}) {
  const parsed = useMemo(
    () => progressMarkdown(current.markdown),
    [current.markdown],
  );
  const root = useRef<HTMLElement>(null);
  const heading = useRef<HTMLDivElement>(null);
  const candidate = useRef<HTMLDivElement>(null);
  const [measured, setMeasured] = useState<{
    markdown: string;
    layout: ProgressLayout;
  } | null>(null);
  const [preview, setPreview] = useState<PreviewTarget | null>(null);
  const [dialogError, setDialogError] = useState<unknown>(null);
  const valid =
    measured?.markdown === current.markdown &&
    measured.layout.revision === current.revision;
  const layout = valid ? measured.layout : null;
  const reportError = useProgressLayoutReport(
    agentId,
    scope,
    current.cached || current.error ? null : layout,
  );
  const approved = !!layout?.fits;

  useLayoutEffect(() => {
    const container = root.current;
    const content = candidate.current;
    const title = heading.current;
    if (!container || !content || !title) return;
    let active = true;
    const invalidate = () => {
      container.dataset.fit = "no";
      setMeasured(null);
    };
    const measure = () => {
      if (!active) return;
      if (!current.revision || document.fonts.status === "loading") {
        invalidate();
        return;
      }
      const viewport = window.visualViewport?.height || window.innerHeight;
      const budget = Math.min(150, Math.max(80, Math.floor(viewport / 4)));
      container.style.setProperty("--progress-height", `${budget}px`);
      const width = Math.max(0, container.clientWidth - 20);
      const height = Math.max(
        0,
        budget - title.getBoundingClientRect().height - 7,
      );
      if (!width || !height) {
        invalidate();
        return;
      }
      const bounds = content.getBoundingClientRect();
      let contentWidth = parsed.supported
        ? Math.max(bounds.width, content.scrollWidth)
        : 0;
      let contentHeight = parsed.supported
        ? Math.max(bounds.height, content.scrollHeight)
        : 0;
      for (const element of content.querySelectorAll<HTMLElement>("*")) {
        const rect = element.getBoundingClientRect();
        contentWidth = Math.max(
          contentWidth,
          rect.right - bounds.left,
          bounds.right - rect.left,
          element.clientWidth ? element.scrollWidth : 0,
        );
        contentHeight = Math.max(
          contentHeight,
          rect.bottom - bounds.top,
          element.clientHeight
            ? rect.top - bounds.top + element.scrollHeight
            : 0,
        );
      }
      const fits =
        parsed.supported &&
        width > 0 &&
        height > 0 &&
        contentWidth <= width + 0.5 &&
        contentHeight <= height + 0.5;
      const next: ProgressLayout = {
        revision: current.revision,
        width,
        height,
        contentWidth,
        contentHeight,
        fits,
        reason: !parsed.supported ? "unsupported" : fits ? null : "overflow",
      };
      // ResizeObserver runs before paint. Hide a now-invalid visible copy before
      // React commits the new notice, including changes in loaded font metrics.
      container.dataset.fit = fits ? "yes" : "no";
      setMeasured((previous) =>
        previous?.markdown === current.markdown &&
        JSON.stringify(previous.layout) === JSON.stringify(next)
          ? previous
          : { markdown: current.markdown, layout: next },
      );
    };
    const observer = new ResizeObserver(measure);
    // The zero-height measurement layer follows width changes but never changes
    // height when the visible copy is accepted or rejected.
    observer.observe(content.parentElement!);
    observer.observe(title);
    observer.observe(content);
    window.addEventListener("resize", measure);
    window.visualViewport?.addEventListener("resize", measure);
    document.fonts.addEventListener("loading", invalidate);
    document.fonts.addEventListener("loadingdone", measure);
    document.fonts.addEventListener("loadingerror", measure);
    void document.fonts.ready.then(() => {
      if (active) measure();
    });
    measure();
    return () => {
      active = false;
      observer.disconnect();
      window.removeEventListener("resize", measure);
      window.visualViewport?.removeEventListener("resize", measure);
      document.fonts.removeEventListener("loading", invalidate);
      document.fonts.removeEventListener("loadingdone", measure);
      document.fonts.removeEventListener("loadingerror", measure);
    };
  }, [current.markdown, current.revision, current.error, parsed]);

  const openOriginal = () => setPreview({ agent: agentId, path: current.path });
  return (
    <>
      <section
        ref={root}
        className="agent-panel"
        aria-label="Agent progress"
        data-agent={agentId}
        data-panel-revision={current.revision ?? undefined}
        data-cached={current.cached ? "yes" : "no"}
        data-fit={approved ? "yes" : "no"}
      >
        <div ref={heading} className="agent-panel-heading">
          <button
            type="button"
            onClick={openOriginal}
            disabled={!current.path}
            aria-label="Open PROGRESS.md"
          >
            <code>PROGRESS.md</code>
          </button>
          {current.cached && <span>Saved copy</span>}
          {Boolean(current.error) && current.markdown.trim() && (
            <>
              <span>Cannot read PROGRESS.md.</span>
              <button
                type="button"
                onClick={() => setDialogError(current.error)}
              >
                Error details
              </button>
              <button type="button" onClick={retry}>
                Retry
              </button>
            </>
          )}
          {Boolean(reportError) && (
            <button type="button" onClick={() => setDialogError(reportError)}>
              Cannot report panel size
            </button>
          )}
        </div>
        <div className="agent-panel-measure" aria-hidden="true" inert>
          <div ref={candidate} className="progress-markdown">
            {parsed.nodes}
          </div>
        </div>
        <div className="agent-panel-content">
          {Boolean(current.error) && !current.markdown.trim() ? (
            <div className="agent-panel-notice" role="alert">
              <span>Cannot read PROGRESS.md.</span>
              <button
                type="button"
                onClick={() => setDialogError(current.error)}
              >
                Error details
              </button>
              <button type="button" onClick={retry}>
                Retry
              </button>
            </div>
          ) : (
            <>
              <div className="agent-panel-notice" role="status">
                {!parsed.supported
                  ? "Progress format is unsupported."
                  : layout?.fits === false
                    ? "Progress does not fit."
                    : "Checking progress layout."}
              </div>
              {approved && (
                <div
                  className={`${current.cached ? "agent-panel-saved" : "agent-panel-current"} progress-markdown`}
                  onClick={(event) => {
                    const link = (event.target as Element).closest("a[href]");
                    // Portals from nested file previews must never reopen an outer link.
                    if (
                      !(link instanceof HTMLAnchorElement) ||
                      !event.currentTarget.contains(link)
                    )
                      return;
                    try {
                      const target = localFileLink(
                        link.getAttribute("href") || "",
                      );
                      if (!target) return;
                      event.preventDefault();
                      event.stopPropagation();
                      setPreview({
                        agent: agentId,
                        ...relativeToDocument(target, current.path),
                      });
                    } catch (error) {
                      event.preventDefault();
                      event.stopPropagation();
                      setDialogError(error);
                    }
                  }}
                >
                  {parsed.nodes}
                </div>
              )}
            </>
          )}
        </div>
      </section>
      <FilePreview target={preview} onClose={() => setPreview(null)} />
      <PreviewModal
        opened={!!dialogError}
        onClose={() => setDialogError(null)}
        title="Progress error"
      >
        <ErrorDescription value={dialogError} />
      </PreviewModal>
    </>
  );
}
