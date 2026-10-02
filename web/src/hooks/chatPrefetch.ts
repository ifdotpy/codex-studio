import { useCallback, useEffect, useRef } from "react";
import {
  prefetchTranscript,
  TRANSCRIPT_PREFETCH_LIMIT,
  watchSyncInvalidations,
  watchTranscriptRevisions,
} from "../sync/client";
import { peekTranscript } from "../sync/transcriptCache";
import { prefetchProgress } from "../components/agents/progressCache";
import { onResume } from "../sync/resume";
import type { Snapshot } from "../types";

// One background slot shares history and progress work. A completed prefetch
// releases its projection and holds no per-chat stream.
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
    let readyChat: string | null = null;
    let preferred: string | null = null;
    let revisions: Record<string, number> | null = null;
    const readyAt = Date.now() + 1000;
    const running = new Set<string>();
    const checked = new Map<string, { version: string; at: number }>();
    const progressChecked = new Map<string, number>();
    const failed = new Map<string, number>();
    const progressFailed = new Map<string, number>();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const schedule = (delay: number) => {
      clearTimeout(timer);
      if (!stopped) timer = setTimeout(pump, delay);
    };
    const pump = () => {
      clearTimeout(timer);
      if (stopped || document.hidden || navigator.onLine === false) return;
      if (Date.now() < readyAt) {
        schedule(readyAt - Date.now());
        return;
      }
      const { data, opened } = current.current;
      if (!data) {
        schedule(1000);
        return;
      }
      const selected = data.threads.find((agent) => agent.id === opened);
      if (selected?.source === "managed" && opened && readyChat !== opened) {
        if (!peekTranscript(workspaceId, opened)) {
          schedule(500);
          return;
        }
        readyChat = opened;
      }
      const root = selected?.isLead ? selected.id : selected?.rootId;
      const now = Date.now();
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
      const historyTargets = mobile
        ? new Set(ranked.slice(0, 2).map((agent) => agent.id))
        : null;
      // Progress files have no transcript revision. Keep their separate checks
      // limited to the nearest switch targets instead of every worker.
      const progressTargets = new Set(
        ranked.slice(0, 31).map((agent) => agent.id),
      );
      const candidates = ranked.map((agent) => {
        const version = revisions
          ? String(revisions[agent.id] ?? 0)
          : JSON.stringify(agent);
        const previous = checked.get(agent.id);
        const interval = agent.inFlight ? 5000 : 60000;
        const changed = previous?.version !== version;
        const historyAt = Math.max(
          failed.get(agent.id) || 0,
          !previous || changed
            ? 0
            : revisions
              ? Infinity
              : previous.at + interval,
        );
        const progressAt = progressTargets.has(agent.id)
          ? Math.max(
              progressFailed.get(agent.id) || 0,
              (progressChecked.get(agent.id) || 0) + interval,
            )
          : Infinity;
        return {
          agent,
          version,
          historyAt:
            historyTargets && !historyTargets.has(agent.id)
              ? Infinity
              : historyAt,
          progressAt,
        };
      });
      for (const { agent, version, historyAt, progressAt } of candidates) {
        if (active >= TRANSCRIPT_PREFETCH_LIMIT) break;
        if (running.has(agent.id) || Math.min(historyAt, progressAt) > now)
          continue;
        active++;
        running.add(agent.id);
        const historyDue = historyAt <= now;
        const progressDue = progressAt <= now;
        void (async () => {
          // Keep at most one request in each background slot.
          const [history] = await Promise.allSettled([
            historyDue
              ? prefetchTranscript(workspaceId, agent.id)
              : Promise.resolve(null),
          ]);
          const [progress] = await Promise.allSettled([
            progressDue
              ? prefetchProgress(data.stateDir, agent.id)
              : Promise.resolve(null),
          ]);
          return [history, progress] as const;
        })()
          .then(([history, progress]) => {
            if (stopped) return;
            if (historyDue) {
              if (history.status === "fulfilled" && history.value) {
                checked.set(agent.id, { version, at: Date.now() });
                failed.delete(agent.id);
              } else
                failed.set(
                  agent.id,
                  Date.now() + (history.status === "rejected" ? 15000 : 3000),
                );
            }
            if (progressDue) {
              if (progress.status === "fulfilled" && progress.value) {
                progressChecked.set(agent.id, Date.now());
                progressFailed.delete(agent.id);
              } else progressFailed.set(agent.id, Date.now() + 15000);
            }
          })
          .finally(() => {
            active--;
            running.delete(agent.id);
            schedule(50);
          });
      }
      const eligible = new Set(data.threads.map((agent) => agent.id));
      for (const records of [checked, progressChecked, failed, progressFailed])
        for (const id of records.keys())
          if (!eligible.has(id)) records.delete(id);
      if (active < TRANSCRIPT_PREFETCH_LIMIT) {
        const due = candidates
          .filter(({ agent }) => !running.has(agent.id))
          .map(({ historyAt, progressAt }) => Math.min(historyAt, progressAt));
        schedule(Math.max(50, Math.min(60000, ...due.map((at) => at - now))));
      }
    };
    const request = (id: string) => {
      if (current.current.opened === id) return;
      preferred = id;
      const previous = checked.get(id);
      // Rehydrate an evicted chat before a pointer click or keyboard activation.
      // Coalesce repeated pointer/focus events for the same fresh page.
      if (
        !peekTranscript(workspaceId, id) ||
        !previous ||
        Date.now() - previous.at > 1000
      )
        checked.delete(id);
      progressChecked.delete(id);
      schedule(0);
    };
    control.current = { request, changed: () => schedule(0) };
    schedule(1000);
    const stopRevisions = watchTranscriptRevisions(workspaceId, (next) => {
      revisions = next;
      schedule(0);
    });
    const stopInvalidations = watchSyncInvalidations("state", () =>
      schedule(0),
    );
    const stopResume = onResume(() => {
      // The resumed stream reports changed revisions. Keep idle chat checks.
      if (!revisions) checked.clear();
      schedule(0);
    });
    return () => {
      stopped = true;
      control.current = null;
      clearTimeout(timer);
      stopRevisions();
      stopInvalidations();
      stopResume();
    };
  }, [workspaceId, data?.stateDir]);
  useEffect(() => {
    control.current?.changed();
  }, [data, opened]);
  return useCallback((id: string) => control.current?.request(id), []);
}
