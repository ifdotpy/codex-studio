import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorText, setToken } from "./api";
import type { Snapshot, Message } from "./types";
export function useSnapshot() {
  const [data, setData] = useState<Snapshot | null>(null),
    [error, setError] = useState("");
  const refresh = useCallback(async () => {
    try {
      const next = await api<Snapshot>("/api/state");
      setToken(next.token);
      setData(next);
      setError("");
    } catch (e) {
      setError(errorText(e));
    }
  }, []);
  useEffect(() => {
    let stopped = false,
      timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      await refresh();
      if (!stopped) timer = setTimeout(poll, 1600);
    };
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [refresh]);
  return { data, error, refresh };
}
export function useMessages(
  id: string | null,
  kind: "agent" | "room" | "legacy",
) {
  const [items, setItems] = useState<Message[]>([]),
    [notice, setNotice] = useState(""),
    [before, setBefore] = useState<number | null>(null);
  const active = useRef(id),
    expanded = useRef(false);
  active.current = id;
  const load = useCallback(async () => {
    if (!id) return;
    try {
      if (kind === "room") {
        const d = await api(`/api/agent-chat?room=${encodeURIComponent(id)}`);
        if (active.current !== id) return;
        setItems((old) =>
          [
            ...new Map(
              [
                ...old,
                ...d.messages.map((m: Message) => ({
                  ...m,
                  role: "assistant",
                })),
              ].map((m) => [m.id, m]),
            ).values(),
          ].sort((a, b) => (a.seq || 0) - (b.seq || 0)),
        );
        if (!expanded.current) setBefore(d.nextBefore);
      } else if (kind === "legacy") {
        const d = await api(`/api/messages?room=${encodeURIComponent(id)}`);
        if (active.current !== id) return;
        setItems(
          d.map((m: Message) => ({
            ...m,
            role: m.author === "user" ? "user" : "assistant",
          })),
        );
      } else {
        const d = await api(`/api/transcript?id=${encodeURIComponent(id)}`);
        if (active.current !== id) return;
        setItems(
          (d.items || []).flatMap((m: Message) =>
            m.inputs
              ? m.inputs.map(
                  (
                    r: { kind: string; text: string; truncated: boolean },
                    i: number,
                  ) => ({
                    ...m,
                    id: `${m.id}:${i}`,
                    role: r.kind === "user" ? "user" : "tool",
                    text: r.text,
                    truncated: r.truncated,
                    title:
                      r.kind === "user"
                        ? "You"
                        : (
                            {
                              agent_message: "Agent message received",
                              child_result: "Worker result received",
                              monitor_exit: "Command finished",
                              monitor_cancelled: "Command cancelled",
                              complaint_response: "Complaint response received",
                              followup: "Agent follow-up",
                              complaint: "Complaint requires a response",
                            } as Record<string, string>
                          )[r.kind] || "Team activity",
                  }),
                )
              : [m],
          ),
        );
        setNotice(
          d.unavailable ||
            (d.truncated
              ? "Recent messages only. Full history remains on disk."
              : ""),
        );
      }
    } catch (e) {
      if (active.current === id) setNotice(errorText(e));
    }
  }, [id, kind]);
  useEffect(() => {
    setItems([]);
    setNotice("");
    setBefore(null);
    expanded.current = false;
    let stopped = false,
      timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      await load();
      if (!stopped) timer = setTimeout(poll, 900);
    };
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [load]);
  const older = async () => {
    if (!id || !before) return;
    const d = await api(
      `/api/agent-chat?room=${encodeURIComponent(id)}&before=${before}`,
    );
    if (active.current !== id) return;
    expanded.current = true;
    setBefore(d.nextBefore);
    setItems((old) =>
      [
        ...new Map(
          [
            ...d.messages.map((m: Message) => ({ ...m, role: "assistant" })),
            ...old,
          ].map((m) => [m.id, m]),
        ).values(),
      ].sort((a, b) => (a.seq || 0) - (b.seq || 0)),
    );
  };
  return { items, notice, before, older, reload: load };
}
