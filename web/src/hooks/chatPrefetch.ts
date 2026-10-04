import { useCallback, useEffect, useRef } from "react";
import {
  prefetchTranscript,
  TRANSCRIPT_PREFETCH_LIMIT,
  watchResourceChanges,
  watchSyncInvalidations,
} from "../sync/client";
import type { ResourceVersion } from "../sync/resourceEvents";
import { peekTranscript, subscribeTranscript } from "../sync/transcriptCache";
import { readProgress } from "../components/agents/progressCache";
import { onResume } from "../sync/resume";
import type { Snapshot } from "../types";
import { foregroundTranscriptPending } from "./chatPrefetchState";

const MAX_TRANSCRIPT_WATCHES = 12;

// History stays warm only while a targeted transcript notification says it
// changed. Progress is fetched lazily for an explicit navigation target.
export function useChatPrefetch(
  data: Snapshot | null,
  opened: string | null,
  workspaceId: string,
) {
  const current = useRef({ data, opened });
  current.current = { data, opened };
  const control = useRef<{
    request: (id: string) => void;
    changed: () => void;
  } | null>(null);

  useEffect(() => {
    if (!workspaceId) return;
    let stopped = false;
    let active = 0;
    let preferred: string | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const running = new Map<string, AbortController>();
    const failedHistory = new Set<string>();
    const pendingHistory = new Set<string>();
    const pendingVersions = new Map<string, ResourceVersion>();
    const runningVersions = new Map<string, ResourceVersion>();
    const pendingProgress = new Set<string>();
    const transcriptStops = new Map<string, () => void>();
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
        (agent) => agent.id === snapshot.opened && agent.source === "managed",
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
          const runningVersion = runningVersions.get(id);
          // Reconnect can echo the cached revision while its refresh is active.
          // A newer revision still queues a followup pull below.
          if (
            version &&
            runningVersion?.epoch === version.epoch &&
            runningVersion?.revision === version.revision
          )
            return;
          failedHistory.delete(id);
          pendingHistory.add(id);
          if (version) pendingVersions.set(id, version);
          schedule();
        },
      );
      transcriptStops.set(id, stop);
    };
    const run = (
      id: string,
      stateDir: string,
      history: boolean,
      progress: boolean,
      version?: ResourceVersion,
    ) => {
      const controller = new AbortController();
      running.set(id, controller);
      if (history && version) runningVersions.set(id, version);
      active++;
      void (async () => {
        try {
          if (history) {
            pendingHistory.delete(id);
            const done = await prefetchTranscript(
              workspaceId,
              id,
              controller.signal,
            );
            if (!done && !controller.signal.aborted) failedHistory.add(id);
          }
          if (progress && !controller.signal.aborted) {
            pendingProgress.delete(id);
            await readProgress(stateDir, id, controller.signal);
          }
        } catch {
          if (!controller.signal.aborted && !pendingHistory.has(id))
            failedHistory.add(id);
        } finally {
          running.delete(id);
          runningVersions.delete(id);
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
          agent.source === "managed" && agent.id !== opened && !agent.archived,
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
      const targets = mobile
        ? new Set(ranked.slice(0, 2).map((agent) => agent.id))
        : new Set(
            ranked.slice(0, MAX_TRANSCRIPT_WATCHES).map((agent) => agent.id),
          );
      for (const [id, stop] of transcriptStops) {
        if (!targets.has(id)) {
          stop();
          transcriptStops.delete(id);
          pendingHistory.delete(id);
          pendingVersions.delete(id);
          failedHistory.delete(id);
          running.get(id)?.abort();
        }
      }
      for (const [id, controller] of running)
        if (!targets.has(id)) controller.abort();
      for (const id of pendingProgress)
        if (!targets.has(id)) pendingProgress.delete(id);
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
        run(
          agent.id,
          data.stateDir,
          true,
          pendingProgress.has(agent.id),
          version,
        );
      }
      if (active >= TRANSCRIPT_PREFETCH_LIMIT) return;
      for (const agent of ranked) {
        if (!pendingProgress.has(agent.id) || running.has(agent.id)) continue;
        if (active >= TRANSCRIPT_PREFETCH_LIMIT) break;
        pendingProgress.delete(agent.id);
        run(agent.id, data.stateDir, false, true);
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
      pendingProgress.add(id);
      schedule();
    };
    control.current = {
      request,
      changed: () => {
        watchForegroundTranscript();
        schedule();
      },
    };
    schedule(1000);
    const stopInvalidations = watchSyncInvalidations("state", () => schedule());
    const stopResume = onResume(() => schedule());
    const stopVisibility = () => {
      if (document.hidden || navigator.onLine === false) {
        for (const controller of running.values()) controller.abort();
        for (const stop of transcriptStops.values()) stop();
        transcriptStops.clear();
      }
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
      stopResume();
      document.removeEventListener("visibilitychange", stopVisibility);
      window.removeEventListener("online", stopVisibility);
      window.removeEventListener("offline", stopVisibility);
    };
  }, [workspaceId, data?.stateDir]);

  useEffect(() => {
    control.current?.changed();
  }, [data, opened]);
  return useCallback((id: string) => control.current?.request(id), []);
}
