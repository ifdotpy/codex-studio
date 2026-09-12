import { useEffect, useRef } from "react";
import { prefetchTranscript, watchSyncInvalidations } from "../sync/client";
import { peekTranscript } from "../sync/transcriptCache";
import { prefetchProgress } from "../components/progressCache";
import { onResume } from "../sync/resume";
import type { Snapshot } from "../types";

// Two background slots share history and progress work. Each slot has at most
// two requests; the selected chat keeps its independent foreground reads.
export function useChatPrefetch(
  data: Snapshot | null,
  opened: string | null,
  workspaceId: string,
) {
  const current = useRef({ data, opened });
  current.current = { data, opened };
  useEffect(() => {
    if (!workspaceId) return;
    let stopped = false;
    let active = 0;
    let readyChat: string | null = null;
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
          agent.source === "managed" &&
          agent.id !== opened &&
          !agent.archived &&
          (agent.isLead || (!!root && agent.rootId === root)),
      );
      // Reserve one of the 32 cache entries for the selected chat. History still
      // covers the full pool, while progress warms the nearest switch targets.
      const progressTargets = new Set(
        [...pool]
          .sort(
            (a, b) =>
              Number(!!b.isLead) - Number(!!a.isLead) ||
              Number(!!root && b.rootId === root) -
                Number(!!root && a.rootId === root) ||
              Number(!!b.inFlight) - Number(!!a.inFlight) ||
              b.created - a.created,
          )
          .slice(0, 31)
          .map((agent) => agent.id),
      );
      const candidates = pool
        .map((agent) => {
          const version = JSON.stringify(agent);
          const previous = checked.get(agent.id);
          const interval = agent.inFlight ? 5000 : 60000;
          const historyAt = Math.max(
            failed.get(agent.id) || 0,
            previous
              ? previous.at + (version !== previous.version ? 3000 : interval)
              : 0,
          );
          const progressAt = progressTargets.has(agent.id)
            ? Math.max(
                progressFailed.get(agent.id) || 0,
                (progressChecked.get(agent.id) || 0) + interval,
              )
            : Infinity;
          return { agent, version, previous, historyAt, progressAt };
        })
        .sort(
          (a, b) =>
            Number(progressTargets.has(b.agent.id)) -
              Number(progressTargets.has(a.agent.id)) ||
            Number(!!b.agent.isLead) - Number(!!a.agent.isLead) ||
            (a.previous?.at || 0) - (b.previous?.at || 0) ||
            b.agent.created - a.agent.created,
        );
      for (const { agent, version, historyAt, progressAt } of candidates) {
        if (active >= 2) break;
        if (running.has(agent.id) || Math.min(historyAt, progressAt) > now)
          continue;
        active++;
        running.add(agent.id);
        const historyDue = historyAt <= now;
        const progressDue = progressAt <= now;
        void Promise.allSettled([
          historyDue
            ? prefetchTranscript(workspaceId, agent.id)
            : Promise.resolve(null),
          progressDue
            ? prefetchProgress(data.stateDir, agent.id)
            : Promise.resolve(null),
        ])
          .then(([history, progress]) => {
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
            schedule(150);
          });
      }
      const eligible = new Set(data.threads.map((agent) => agent.id));
      for (const records of [checked, progressChecked, failed, progressFailed])
        for (const id of records.keys())
          if (!eligible.has(id)) records.delete(id);
      if (active < 2) {
        const due = candidates
          .filter(({ agent }) => !running.has(agent.id))
          .map(({ historyAt, progressAt }) => Math.min(historyAt, progressAt));
        // File edits need no transcript event. One timer still refreshes them at
        // the same 5-second active and 60-second idle cadence.
        schedule(Math.max(150, Math.min(60000, ...due.map((at) => at - now))));
      }
    };
    schedule(1000);
    const stopInvalidations = watchSyncInvalidations(pump);
    const stopResume = onResume(pump);
    return () => {
      stopped = true;
      clearTimeout(timer);
      stopInvalidations();
      stopResume();
    };
  }, [workspaceId, data?.stateDir]);
}
