import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorText, setToken } from "./api";
import { watchProjection, UnsupportedSyncError } from "./sync/client";
import type { Snapshot, Message, Agent, Json } from "./types";
export function useSnapshot() {
  const [data, setData] = useState<Snapshot | null>(null),
    [error, setError] = useState("");
  const [syncError, setSyncError] = useState("");
  const generation = useRef(0);
  const replicated = useRef(false);
  const refresh = useCallback(async () => {
    const request = ++generation.current;
    try {
      const next = await api<Snapshot>("/api/state");
      if (request !== generation.current) return;
      setToken(next.token);
      if (!replicated.current) setData(next);
      setError("");
    } catch (e) {
      if (request !== generation.current) return;
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
  useEffect(() => {
    let stopped = false,
      dispose = () => {};
    void watchProjection(
      "state",
      (next) => {
        if (!stopped && next) {
          replicated.current = true;
          setSyncError("");
          setData((old) => ({ ...next, token: old?.token || "" }));
        }
      },
      (error) => {
        if (!stopped) setSyncError(errorText(error));
      },
    )
      .then((stop) => {
        if (stopped) stop();
        else dispose = stop;
      })
      .catch((error) => {
        if (!stopped && !(error instanceof UnsupportedSyncError))
          setSyncError(errorText(error));
      });
    return () => {
      stopped = true;
      dispose();
    };
  }, []);
  return { data, error: error || syncError, refresh };
}
export function useMessages(
  id: string | null,
  kind: "agent" | "room" | "legacy",
  managed = false,
  workspace = "",
) {
  const scope = JSON.stringify([workspace, kind, managed, id]);
  const history = useRef(
    new Map<string, { items: Message[]; notice: string; size: number }>(),
  );
  const messageSizes = useRef(new WeakMap<Message, number>());
  const cached =
    managed && kind === "agent" ? history.current.get(scope) : undefined;
  const [items, setItems] = useState<Message[]>([]),
    [loadedId, setLoadedId] = useState<string | null>(null),
    [notice, setNotice] = useState(""),
    [before, setBefore] = useState<number | null>(null),
    [liveAgent, setLiveAgent] = useState<Partial<Agent> | null>(null),
    [connection, setConnection] = useState(""),
    [syncId, setSyncId] = useState<string | null>(null);
  const revision = useRef(0);
  const syncActive = useRef<string | null>(null);
  const active = useRef(scope),
    expanded = useRef(false);
  active.current = scope;
  const accept = useCallback(
    (d: Json) => {
      revision.current++;
      setLoadedId(scope);
      setLiveAgent(d.agent || null);
      const nextItems: Message[] = (d.items || []).flatMap((m: Message) =>
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
      );
      const nextNotice =
        d.unavailable ||
        (d.truncated
          ? "Recent messages only. Full history remains on disk."
          : "");
      setItems(nextItems);
      setNotice(nextNotice);
      // Keep only bounded, display-only history. Current agent state always comes
      // from a fresh response or the parent snapshot when a chat is reopened.
      history.current.delete(scope);
      if (
        !d.unavailable &&
        managed &&
        kind === "agent" &&
        nextItems.length <= 4000
      ) {
        let size = 4;
        for (const item of nextItems) {
          let itemSize = messageSizes.current.get(item);
          if (itemSize === undefined) {
            itemSize = JSON.stringify(item).length * 2;
            messageSizes.current.set(item, itemSize);
          }
          size += itemSize + 2;
        }
        if (size <= 4_000_000)
          history.current.set(scope, {
            items: nextItems,
            notice: nextNotice,
            size,
          });
        let bytes = 0,
          count = 0;
        for (const value of history.current.values()) {
          bytes += value.size;
          count += value.items.length;
        }
        while (
          history.current.size > 12 ||
          bytes > 12_000_000 ||
          count > 12000
        ) {
          const oldest = history.current.keys().next().value!;
          const value = history.current.get(oldest)!;
          bytes -= value.size;
          count -= value.items.length;
          history.current.delete(oldest);
        }
      }
    },
    [scope, managed, kind],
  );
  const load = useCallback(
    async (allowed: () => boolean = () => true) => {
      if (!id) return;
      const request = ++revision.current;
      const current = () =>
        active.current === scope &&
        syncActive.current !== scope &&
        request === revision.current &&
        allowed();
      try {
        if (kind === "room") {
          const d = await api(`/api/agent-chat?room=${encodeURIComponent(id)}`);
          if (!current()) return;
          setLoadedId(scope);
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
          if (!current()) return;
          setLoadedId(scope);
          setItems(
            d.map((m: Message) => ({
              ...m,
              role: m.author === "user" ? "user" : "assistant",
            })),
          );
        } else {
          const d = await api(`/api/transcript?id=${encodeURIComponent(id)}`);
          if (!current()) return;
          accept(d);
        }
      } catch (e) {
        if (current()) {
          setLoadedId(scope);
          setNotice(errorText(e));
        }
      }
    },
    [id, kind, accept, scope],
  );
  useEffect(() => {
    if (
      syncId === scope &&
      syncActive.current === scope &&
      id &&
      managed &&
      kind === "agent"
    )
      return;
    const retained =
      managed && kind === "agent" ? history.current.get(scope) : undefined;
    setLoadedId(retained ? scope : null);
    setItems(retained?.items || []);
    syncActive.current = null;
    setLiveAgent(null);
    setConnection("");
    setNotice(retained?.notice || "");
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
    // Do not leave a newly opened chat blank for seconds when SSE is delayed.
    timer = setTimeout(fallback, 200);
    const connect = () => {
      if (stopped) return;
      source?.close();
      source = new EventSource(
        `/api/transcript/stream?id=${encodeURIComponent(id)}`,
      );
      source.onmessage = (event) => {
        if (stopped || active.current !== scope || syncActive.current === scope)
          return;
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
        setLoadedId(scope);
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
  }, [load, id, kind, managed, accept, syncId, scope]);
  useEffect(() => {
    if (!id || kind !== "agent" || !managed) return;
    let stopped = false,
      seen = false,
      dispose = () => {};
    void watchProjection(
      `transcript:${id}`,
      (next) => {
        if (!stopped && active.current === scope && next) {
          seen = true;
          syncActive.current = scope;
          setSyncId(scope);
          accept(next);
        } else if (!stopped && seen && active.current === scope) {
          accept({
            items: [],
            unavailable: "This session is no longer available.",
          });
        }
      },
      () => {},
    )
      .then((stop) => {
        if (stopped) stop();
        else dispose = stop;
      })
      .catch(() => {});
    return () => {
      stopped = true;
      dispose();
    };
  }, [id, kind, managed, accept, scope]);
  const older = async () => {
    if (!id || !before) return;
    const d = await api(
      `/api/agent-chat?room=${encodeURIComponent(id)}&before=${before}`,
    );
    if (active.current !== scope) return;
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
    loaded: !id || loadedId === scope || !!cached,
    items: loadedId === scope ? items : cached?.items || [],
    notice: loadedId === scope ? notice : cached?.notice || "",
    before: loadedId === scope ? before : null,
    older,
    reload: load,
    liveAgent: loadedId === scope ? liveAgent : null,
    connection: loadedId === scope ? connection : "",
  };
}
