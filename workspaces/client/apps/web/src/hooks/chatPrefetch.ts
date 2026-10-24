import {
  useServerSync,
  useServerResources,
  useServerTranscripts,
} from "../servers/ServerServicesContext";
import { useCallback, useEffect, useMemo, useRef } from "react";
import "../sync/client";
import type { ResourceVersion } from "../sync/resourceEvents";
import "../sync/transcriptCache";
import type { Snapshot } from "../types";
import {
  foregroundTranscriptPending,
  TranscriptRefreshGate,
} from "./chatPrefetchState";

// History stays warm only while a targeted transcript notification says it
// changed.
export function useChatPrefetch(
  data: Snapshot | null,
  opened: string | null,
  workspaceId: string,
  enabled = true,
) {
  const {
    prefetchTranscript,
    TRANSCRIPT_PREFETCH_LIMIT,
    watchSyncInvalidations,
  } = useServerSync();
  const { watchResourceChanges } = useServerResources();
  const { peekTranscript, subscribeTranscript } = useServerTranscripts();

  const current = useRef({ data, opened });
  current.current = { data, opened };
  const control = useRef<{
    request: (id: string) => void;
    changed: () => void;
    suspend: (ids: ReadonlySet<string>) => void;
    resume: (ids: ReadonlySet<string>) => void;
  } | null>(null);

  useEffect(() => {
    if (!workspaceId || !enabled) return;
    let stopped = false;
    let active = 0;
    let preferred: string | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const running = new Map<string, AbortController>();
    const failedHistory = new Set<string>();
    const pendingHistory = new Set<string>();
    const pendingVersions = new Map<string, ResourceVersion>();
    const refreshGate = new TranscriptRefreshGate();
    const transcriptStops = new Map<string, () => void>();
    const suppressed = new Set<string>();
    let foregroundId: string | null | undefined;
    let foregroundReady = false;
    let stopForegroundCache: (() => void) | undefined;

    const schedule = (delay = 0) => {
      clearTimeout(timer);
      if (!stopped) timer = setTimeout(pump, delay);
    };
    const watchForegroundTranscript = () => {
      const snapshot = current.current;
      const id = snapshot.data?.threads.some(
        (agent) =>
          agent.id === snapshot.opened &&
          agent.source === "managed" &&
          !suppressed.has(agent.id),
      )
        ? snapshot.opened
        : null;
      if (foregroundId === id) return;
      stopForegroundCache?.();
      stopForegroundCache = undefined;
      foregroundId = id;
      foregroundReady = !id || !!peekTranscript(workspaceId, id);
      if (!id || foregroundReady) return;
      stopForegroundCache = subscribeTranscript(workspaceId, id, () => {
        foregroundReady = true;
        schedule();
      });
    };
    const watchTranscript = (id: string) => {
      if (transcriptStops.has(id)) return;
      const stop = watchResourceChanges(
        { kind: "transcript", agentId: id },
        (version) => {
          // Inline intervals are persisted before this notification is delivered.
          if (
            version?.transcriptSequence !== undefined &&
            (peekTranscript(workspaceId, id)?.seq ?? 0) >=
              version.transcriptSequence
          ) {
            pendingHistory.delete(id);
            pendingVersions.delete(id);
            failedHistory.delete(id);
            return;
          }
          // Ordinary stream frames carry their data. Only a reported recovery
          // boundary permits an HTTP interval read.
          if (
            !version?.reason ||
            (version.reason === "change" && !version.transcriptReadRequired) ||
            version.transcriptSequence !== undefined
          )
            return;
          // Reconnect can echo the cached revision while its refresh is active.
          // A newer revision still queues a followup pull below.
          if (refreshGate.shouldSuppress(id, version)) return;
          failedHistory.delete(id);
          pendingHistory.add(id);
          if (version) pendingVersions.set(id, version);
          schedule();
        },
      );
      transcriptStops.set(id, stop);
    };
    const run = (id: string, version?: ResourceVersion) => {
      const controller = new AbortController();
      running.set(id, controller);
      refreshGate.start(id, version);
      active++;
      void (async () => {
        let historySucceeded = false;
        try {
          pendingHistory.delete(id);
          const done = await prefetchTranscript(
            workspaceId,
            id,
            controller.signal,
          );
          historySucceeded = done;
          if (!done && !controller.signal.aborted && !pendingHistory.has(id))
            failedHistory.add(id);
          refreshGate.historySettled(id);
        } catch {
          if (!controller.signal.aborted && !pendingHistory.has(id))
            failedHistory.add(id);
        } finally {
          running.delete(id);
          const retryAfterFailure = refreshGate.finish(
            id,
            historySucceeded,
            controller.signal.aborted,
          );
          if (
            retryAfterFailure &&
            !historySucceeded &&
            !controller.signal.aborted &&
            !pendingHistory.has(id)
          ) {
            failedHistory.delete(id);
            pendingHistory.add(id);
          }
          active--;
          schedule();
        }
      })();
    };
    const pump = () => {
      clearTimeout(timer);
      if (stopped || document.hidden || navigator.onLine === false) return;
      const { data, opened } = current.current;
      if (!data || (!opened && data.threads.length > 0)) return;
      watchForegroundTranscript();
      if (foregroundTranscriptPending(foregroundId ?? null, foregroundReady))
        return;
      const selected = data.threads.find((agent) => agent.id === opened);
      const root = selected?.isLead ? selected.id : selected?.rootId;
      const pool = data.threads.filter(
        (agent) =>
          agent.source === "managed" &&
          agent.id !== opened &&
          !agent.archived &&
          !suppressed.has(agent.id),
      );
      const ranked = [...pool].sort(
        (a, b) =>
          Number(b.id === preferred) - Number(a.id === preferred) ||
          Number(!!b.pinned) - Number(!!a.pinned) ||
          Number(!!root && b.rootId === root) -
            Number(!!root && a.rootId === root) ||
          Number(!!b.isLead) - Number(!!a.isLead) ||
          Number(!!b.inFlight) - Number(!!a.inFlight) ||
          Number(b.updated || b.created) - Number(a.updated || a.created),
      );
      const mobile = window.matchMedia("(max-width: 760px)").matches;
      // Keep only the top-ranked offscreen history warm; foreground chat has
      // its own subscription and switching to another target can fetch cold.
      const targets = new Set(
        ranked.slice(0, mobile ? 2 : 1).map((agent) => agent.id),
      );
      for (const [id, stop] of transcriptStops) {
        if (!targets.has(id)) {
          stop();
          transcriptStops.delete(id);
          pendingHistory.delete(id);
          pendingVersions.delete(id);
          refreshGate.forget(id);
          failedHistory.delete(id);
          running.get(id)?.abort();
        }
      }
      for (const [id, controller] of running)
        if (!targets.has(id)) controller.abort();
      const candidates = ranked.filter(
        (agent) =>
          targets.has(agent.id) &&
          pendingHistory.has(agent.id) &&
          !failedHistory.has(agent.id) &&
          !running.has(agent.id),
      );
      for (const agent of ranked)
        if (targets.has(agent.id)) watchTranscript(agent.id);
      for (const agent of candidates) {
        if (active >= TRANSCRIPT_PREFETCH_LIMIT) break;
        const version = pendingVersions.get(agent.id);
        pendingVersions.delete(agent.id);
        run(agent.id, version);
      }
      for (const id of pendingHistory)
        if (!targets.has(id)) {
          pendingHistory.delete(id);
          pendingVersions.delete(id);
        }
    };
    const request = (id: string) => {
      if (current.current.opened === id) return;
      preferred = id;
      if (!peekTranscript(workspaceId, id)) {
        failedHistory.delete(id);
        pendingHistory.add(id);
      }
      schedule();
    };
    const suspend = (ids: ReadonlySet<string>) => {
      for (const id of ids) {
        suppressed.add(id);
        running.get(id)?.abort();
        transcriptStops.get(id)?.();
        transcriptStops.delete(id);
        pendingHistory.delete(id);
        pendingVersions.delete(id);
        refreshGate.forget(id);
        failedHistory.delete(id);
        if (foregroundId === id) {
          stopForegroundCache?.();
          stopForegroundCache = undefined;
          foregroundId = null;
          foregroundReady = true;
        }
      }
    };
    const resume = (ids: ReadonlySet<string>) => {
      for (const id of ids) suppressed.delete(id);
      schedule();
    };
    control.current = {
      request,
      changed: () => {
        watchForegroundTranscript();
        schedule();
      },
      suspend,
      resume,
    };
    schedule(1000);
    const stopInvalidations = watchSyncInvalidations("state", () => schedule());
    const stopVisibility = () => {
      if (document.hidden || navigator.onLine === false) {
        for (const controller of running.values()) controller.abort();
        for (const [id, stop] of transcriptStops) {
          stop();
          refreshGate.forget(id);
        }
        transcriptStops.clear();
        pendingHistory.clear();
        pendingVersions.clear();
      }
      // Returning only restores subscriptions; their stream baseline owns data.
      schedule();
    };
    document.addEventListener("visibilitychange", stopVisibility);
    window.addEventListener("online", stopVisibility);
    window.addEventListener("offline", stopVisibility);
    return () => {
      stopped = true;
      control.current = null;
      clearTimeout(timer);
      stopForegroundCache?.();
      for (const controller of running.values()) controller.abort();
      for (const stop of transcriptStops.values()) stop();
      transcriptStops.clear();
      stopInvalidations();
      document.removeEventListener("visibilitychange", stopVisibility);
      window.removeEventListener("online", stopVisibility);
      window.removeEventListener("offline", stopVisibility);
    };
  }, [workspaceId, data?.stateDir, enabled]);

  useEffect(() => {
    control.current?.changed();
  }, [data, opened]);
  const request = useCallback((id: string) => control.current?.request(id), []);
  const suspend = useCallback(
    (ids: ReadonlySet<string>) => control.current?.suspend(ids),
    [],
  );
  const resume = useCallback(
    (ids: ReadonlySet<string>) => control.current?.resume(ids),
    [],
  );
  return useMemo(
    () => Object.assign(request, { suspend, resume }),
    [request, resume, suspend],
  );
}
