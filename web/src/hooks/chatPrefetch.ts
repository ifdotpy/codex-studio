import { useEffect, useRef } from "react";
import { prefetchTranscript, watchSyncInvalidations } from "../sync/client";
import { peekTranscript } from "../sync/transcriptCache";
import { onResume } from "../sync/resume";
import type { Snapshot } from "../types";

// Warm other conversations without adding a socket or a timer for each chat.
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
    const failed = new Map<string, number>();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const pump = () => {
      if (
        stopped ||
        document.hidden ||
        navigator.onLine === false ||
        Date.now() < readyAt
      )
        return;
      const { data, opened } = current.current;
      if (!data) return;
      const selected = data.threads.find((agent) => agent.id === opened);
      if (selected?.source === "managed" && opened && readyChat !== opened) {
        if (!peekTranscript(workspaceId, opened)) return;
        readyChat = opened;
      }
      const root = selected?.isLead ? selected.id : selected?.rootId;
      const now = Date.now();
      const candidates = data.threads
        .filter(
          (agent) =>
            agent.source === "managed" &&
            agent.id !== opened &&
            !agent.archived &&
            (agent.isLead || (!!root && agent.rootId === root)),
        )
        .map((agent) => ({
          agent,
          version: JSON.stringify(agent),
          previous: checked.get(agent.id),
        }))
        .filter(
          ({ agent, version, previous }) =>
            !running.has(agent.id) &&
            now >= (failed.get(agent.id) || 0) &&
            (!previous ||
              (now - previous.at >= 3000 && version !== previous.version) ||
              now - previous.at >= (agent.inFlight ? 5000 : 60000)),
        )
        .sort(
          (a, b) =>
            Number(!!b.agent.isLead) - Number(!!a.agent.isLead) ||
            (a.previous?.at || 0) - (b.previous?.at || 0) ||
            Number(b.agent.created || 0) - Number(a.agent.created || 0),
        );
      for (const { agent, version } of candidates) {
        if (active >= 2) break;
        active++;
        running.add(agent.id);
        void prefetchTranscript(workspaceId, agent.id)
          .then((done) => {
            if (done) checked.set(agent.id, { version, at: Date.now() });
            else failed.set(agent.id, Date.now() + 3000);
          })
          .catch(() => {
            // The selected chat owns visible connection errors and retries.
            failed.set(agent.id, Date.now() + 15000);
          })
          .finally(() => {
            active--;
            running.delete(agent.id);
            // Yield between batches so foreground rendering and sends run first.
            if (!stopped) {
              clearTimeout(timer);
              timer = setTimeout(pump, 150);
            }
          });
      }
      const eligible = new Set(data.threads.map((agent) => agent.id));
      for (const id of checked.keys())
        if (!eligible.has(id)) checked.delete(id);
      for (const id of failed.keys()) if (!eligible.has(id)) failed.delete(id);
    };
    // The first conversation gets the connection before background history.
    timer = setTimeout(pump, 1000);
    const stopInvalidations = watchSyncInvalidations(pump);
    const stopResume = onResume(pump);
    return () => {
      stopped = true;
      clearTimeout(timer);
      stopInvalidations();
      stopResume();
    };
  }, [workspaceId]);
}
