import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorText, setToken } from "./api";
import type { Snapshot, Message, Agent, Json } from "./types";
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
  managed = false,
) {
  const [items, setItems] = useState<Message[]>([]),
    [loadedId, setLoadedId] = useState<string | null>(null),
    [notice, setNotice] = useState(""),
    [before, setBefore] = useState<number | null>(null),
    [liveAgent, setLiveAgent] = useState<Partial<Agent> | null>(null),
    [connection, setConnection] = useState("");
  const active = useRef(id),
    expanded = useRef(false);
  active.current = id;
  const accept = useCallback((d: Json) => {
    setLoadedId(active.current);
    setLiveAgent(d.agent || null);
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
                sourceId: m.id,
                assets: (r as Json).assets || (i === 0 ? m.assets : []),
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
  }, []);
  const load = useCallback(
    async (allowed: () => boolean = () => true) => {
      if (!id) return;
      try {
        if (kind === "room") {
          const d = await api(`/api/agent-chat?room=${encodeURIComponent(id)}`);
          if (active.current !== id || !allowed()) return;
          setLoadedId(id);
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
          if (active.current !== id || !allowed()) return;
          setLoadedId(id);
          setItems(
            d.map((m: Message) => ({
              ...m,
              role: m.author === "user" ? "user" : "assistant",
            })),
          );
        } else {
          const d = await api(`/api/transcript?id=${encodeURIComponent(id)}`);
          if (active.current !== id || !allowed()) return;
          accept(d);
        }
      } catch (e) {
        if (active.current === id && allowed()) {
          setLoadedId(id);
          setNotice(errorText(e));
        }
      }
    },
    [id, kind, accept],
  );
  useEffect(() => {
    setLoadedId(null);
    setItems([]);
    setLiveAgent(null);
    setConnection("");
    setNotice("");
    setBefore(null);
    expanded.current = false;
    let stopped = false;
    let streamLive = false;
    let polling = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      if (polling || stopped || streamLive) return;
      polling = true;
      await load(() => !stopped && !streamLive);
      polling = false;
      if (!stopped && !streamLive)
        timer = setTimeout(poll, managed ? 2000 : 900);
    };
    if (!id || !managed || kind !== "agent") {
      void poll();
      return () => {
        stopped = true;
        clearTimeout(timer);
      };
    }
    let records = new Map<string, Message>();
    let source: EventSource;
    const fallback = () => {
      clearTimeout(timer);
      if (!stopped) void poll();
    };
    timer = setTimeout(fallback, 3000);
    const connect = () => {
      if (stopped) return;
      source?.close();
      source = new EventSource(
        `/api/transcript/stream?id=${encodeURIComponent(id)}`,
      );
      source.onmessage = (event) => {
        if (stopped || active.current !== id) return;
        try {
          const d = JSON.parse(event.data);
          if (d.replace) records = new Map();
          for (const item of d.items) records.set(item.id, item);
          records = new Map(
            d.order.map((key: string) => [key, records.get(key)]),
          );
          clearTimeout(timer);
          streamLive = true;
          setConnection("live");
          accept({ ...d, items: [...records.values()] });
        } catch {
          streamLive = false;
          setConnection("reconnecting");
          fallback();
        }
      };
      source.onerror = () => {
        if (stopped) return;
        streamLive = false;
        setConnection("reconnecting");
        fallback();
      };
      source.addEventListener("unavailable", (event) => {
        source.close();
        stopped = true;
        clearTimeout(timer);
        setLoadedId(id);
        setConnection("unavailable");
        setNotice(JSON.parse((event as MessageEvent).data).error);
      });
    };
    const offline = () => {
      source.close();
      streamLive = false;
      setConnection("reconnecting");
      fallback();
    };
    connect();
    window.addEventListener("offline", offline);
    window.addEventListener("online", connect);
    return () => {
      stopped = true;
      clearTimeout(timer);
      source.close();
      window.removeEventListener("offline", offline);
      window.removeEventListener("online", connect);
    };
  }, [load, id, kind, managed, accept]);
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
  return {
    items: loadedId === id ? items : [],
    notice: loadedId === id ? notice : "",
    before: loadedId === id ? before : null,
    older,
    reload: load,
    liveAgent: loadedId === id ? liveAgent : null,
    connection,
  };
}
