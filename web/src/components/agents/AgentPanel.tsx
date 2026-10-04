import {
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { NetworkTimeoutError } from "../../api";
import {
  watchResourceChanges,
  watchResourceConnection,
  type ResourceConnectionState,
} from "../../sync/client";
import ErrorDescription from "../ErrorDescription";
import FilePreview, { type PreviewTarget } from "../FilePreview";
import PreviewModal from "../PreviewModal";
import { localFileLink } from "../fileLinks";
import { relativeToDocument } from "../filePreviewFormats";
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
  const [connection, setConnection] =
    useState<ResourceConnectionState>("connecting");
  const refresh = useRef<(() => void) | undefined>(undefined);

  useEffect(() => {
    let active = true;
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
        if (active && visible() && refreshPending) {
          refreshPending = false;
          queueMicrotask(() => void read());
        }
      }
    };
    const wake = () => {
      void read();
    };
    refresh.current = wake;
    const stopResourceChanges = watchResourceChanges(
      { kind: "panel", agentId },
      wake,
    );
    const stopConnection = watchResourceConnection(setConnection);
    const visibility = () => {
      if (!document.hidden && navigator.onLine !== false) return;
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
    return () => {
      active = false;
      request?.controller.abort();
      stopResourceChanges();
      stopConnection();
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
      connection={connection}
      agentId={agentId}
      scope={scope}
      stateDir={stateDir}
      retry={() => refresh.current?.()}
    />
  );
}

function ProgressDisplay({
  current,
  connection,
  agentId,
  scope,
  stateDir,
  retry,
}: {
  current: ProgressState;
  connection: ResourceConnectionState;
  agentId: string;
  scope: string;
  stateDir: string;
  retry: () => void;
}) {
  const parsed = useMemo(
    () => progressMarkdown(current.markdown),
    [current.markdown],
  );
  const contentId = useId();
  const root = useRef<HTMLElement>(null);
  const visibleContent = useRef<HTMLDivElement>(null);
  const heading = useRef<HTMLDivElement>(null);
  const candidate = useRef<HTMLDivElement>(null);
  const [measured, setMeasured] = useState<{
    markdown: string;
    layout: ProgressLayout;
  } | null>(null);
  const [expanded, setExpanded] = useState(false);
  // This device's workspace choice applies across chats to preserve chat space.
  const preferenceKey = `codex-progress-hidden:${stateDir}`;
  const [hiddenChoice, setHiddenChoice] = useState<boolean | null>(() => {
    try {
      const stored = localStorage.getItem(preferenceKey);
      return stored === "true" ? true : stored === "false" ? false : null;
    } catch {
      return null;
    }
  });
  const [narrow, setNarrow] = useState(
    () => window.matchMedia("(max-width: 760px)").matches,
  );
  const hidden = hiddenChoice ?? narrow;
  const [seenRevision, setSeenRevision] = useState(current.revision);
  const changed =
    hidden && seenRevision !== null && current.revision !== seenRevision;
  useEffect(() => {
    const media = window.matchMedia("(max-width: 760px)");
    const resize = () => setNarrow(media.matches);
    media.addEventListener("change", resize);
    return () => media.removeEventListener("change", resize);
  }, []);
  useEffect(() => {
    if (!hidden || seenRevision === null) setSeenRevision(current.revision);
  }, [hidden, current.revision, seenRevision]);
  const showOrHide = (value: boolean) => {
    setSeenRevision(current.revision);
    setHiddenChoice(value);
    setExpanded(false);
    try {
      localStorage.setItem(preferenceKey, String(value));
    } catch {
      /* The control remains usable when browser storage is unavailable. */
    }
  };
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
  const clipped = layout?.reason === "overflow";

  useLayoutEffect(() => {
    const container = root.current;
    const content = candidate.current;
    const title = heading.current;
    if (!container || !content || !title) return;
    let active = true;
    const invalidate = () => {
      setMeasured(null);
    };
    const measure = () => {
      if (!active) return;
      if (!current.revision || document.fonts.status === "loading") {
        invalidate();
        return;
      }
      const viewport = window.visualViewport?.height || window.innerHeight;
      const budget = Math.min(280, Math.max(100, Math.floor(viewport * 0.28)));
      container.style.setProperty("--progress-height", `${budget}px`);
      container.style.setProperty(
        "--progress-expanded-height",
        `${Math.floor(viewport / 2)}px`,
      );
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
      const overflowX = Math.max(0, contentWidth - width);
      const overflowY = Math.max(0, contentHeight - height);
      // Count complete top-level paragraphs/headings and direct list items.
      // Leave the fade out of the fully visible area when content is clipped.
      const lines = Array.from(
        content.querySelectorAll<HTMLElement>(
          ":scope > :not(ul):not(ol), :scope > ul > li, :scope > ol > li",
        ),
      );
      const visible = lines.filter((line) => {
        const rect = line.getBoundingClientRect();
        return (
          rect.bottom - bounds.top <= height - (fits ? 0 : 12) + 0.5 &&
          rect.right - bounds.left <= width + 0.5 &&
          rect.left >= bounds.left - 0.5
        );
      });
      const label = (line?: HTMLElement) => {
        const text = line?.textContent?.replace(/\s+/g, " ").trim();
        return text ? Array.from(text).slice(0, 200).join("") : null;
      };
      const next: ProgressLayout = {
        revision: current.revision,
        width,
        height,
        contentWidth,
        contentHeight,
        overflowX,
        overflowY,
        totalLines: lines.length,
        visibleLines: visible.length,
        lastVisibleLine: label(visible.at(-1)),
        lastVisibleHeading: label(
          visible.filter((line) => /^H[1-6]$/.test(line.tagName)).at(-1),
        ),
        fits,
        reason: !parsed.supported ? "unsupported" : fits ? null : "overflow",
      };
      setMeasured((previous) =>
        previous?.markdown === current.markdown &&
        JSON.stringify(previous.layout) === JSON.stringify(next)
          ? previous
          : { markdown: current.markdown, layout: next },
      );
    };
    const observer = new ResizeObserver(measure);
    // Report Preview geometry in all three states. Hiding is a user choice,
    // not a content defect that should cause the agent to rewrite the file.
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

  useLayoutEffect(() => {
    const content = visibleContent.current;
    if (!content) return;
    const bounds = content.getBoundingClientRect();
    for (const link of content.querySelectorAll<HTMLAnchorElement>("a[href]")) {
      const rect = link.getBoundingClientRect();
      link.tabIndex =
        (expanded && clipped) ||
        (rect.top >= bounds.top &&
          rect.bottom <= bounds.bottom - (clipped ? 12 : 0))
          ? 0
          : -1;
    }
  }, [layout, expanded, clipped, hidden, current.markdown]);

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
        data-fit={layout?.fits ? "yes" : "no"}
        data-clipped={clipped ? "yes" : "no"}
        data-expanded={expanded && clipped ? "yes" : "no"}
        data-hidden={hidden ? "yes" : "no"}
      >
        {hidden && (
          <div className="agent-panel-compact">
            <button
              type="button"
              onClick={openOriginal}
              disabled={!current.path}
              aria-label="Open PROGRESS.md"
            >
              <code>PROGRESS.md</code>
            </button>
            <span className="agent-panel-summary">
              {parsed.firstLine ||
                (current.error
                  ? "Cannot read PROGRESS.md."
                  : "Progress format is unsupported.")}
            </span>
            {changed && (
              <span
                className="agent-panel-changed"
                role="status"
                aria-label="Progress changed"
                title="Progress changed"
              >
                •
              </span>
            )}
            {current.cached && (
              <span className="agent-panel-compact-saved">Saved copy</span>
            )}
            {connection !== "live" && (
              <span className="agent-panel-compact-saved" role="status">
                {connection === "offline" ? "Updates offline" : "Reconnecting"}
              </span>
            )}
            {Boolean(current.error || reportError) && (
              <button
                type="button"
                className="agent-panel-compact-error"
                aria-label={
                  current.error
                    ? "Cannot read PROGRESS.md. Error details"
                    : "Cannot report panel size"
                }
                onClick={() => setDialogError(current.error || reportError)}
              >
                !
              </button>
            )}
            <button
              type="button"
              aria-expanded={false}
              aria-controls={contentId}
              onClick={() => showOrHide(false)}
            >
              Show
            </button>
          </div>
        )}
        <div
          ref={heading}
          className="agent-panel-heading"
          aria-hidden={hidden || undefined}
          inert={hidden}
        >
          <button
            type="button"
            onClick={openOriginal}
            disabled={!current.path}
            aria-label="Open PROGRESS.md"
          >
            <code>PROGRESS.md</code>
          </button>
          {current.cached && <span>Saved copy</span>}
          {connection !== "live" && (
            <span role="status">
              {connection === "offline" ? "Updates offline" : "Reconnecting"}
            </span>
          )}
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
          <div className="agent-panel-controls">
            <button
              type="button"
              aria-expanded={!hidden}
              aria-controls={contentId}
              onClick={() => showOrHide(true)}
            >
              Hide
            </button>
            {parsed.supported && (
              <button
                type="button"
                className="agent-panel-expand"
                style={{
                  visibility: !hidden && clipped ? "visible" : "hidden",
                }}
                disabled={!clipped}
                aria-expanded={expanded && clipped}
                aria-controls={contentId}
                onClick={() => setExpanded((value) => !value)}
              >
                {expanded && clipped ? "Collapse" : "Expand"}
              </button>
            )}
          </div>
        </div>
        <div className="agent-panel-measure" aria-hidden="true" inert>
          <div ref={candidate} className="progress-markdown">
            {parsed.nodes}
          </div>
        </div>
        <div
          ref={visibleContent}
          id={contentId}
          className="agent-panel-content"
          hidden={hidden}
          tabIndex={expanded && clipped ? 0 : undefined}
        >
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
              {!parsed.supported && (
                <div className="agent-panel-notice" role="status">
                  Progress format is unsupported.
                </div>
              )}
              {parsed.supported && (
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
