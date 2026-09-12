import { retainTranscriptItems } from "./transcriptIdentity";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { syncApi as api, ApiError, errorText, setToken } from "./api";
import {
  subscribeProjection,
  syncDatabase,
  UnsupportedSyncError,
} from "./sync/client";
import { peekTranscript, subscribeTranscript } from "./sync/transcriptCache";
import { onResume } from "./sync/resume";
import type { Snapshot, Message, Agent, Json } from "./types";
export function useSnapshot() {
  const [data, setData] = useState<Snapshot | null>(null),
    [error, setError] = useState("");
  const [syncError, setSyncError] = useState("");
  const [workspaceId, setWorkspaceId] = useState("");
  const generation = useRef(0);
  const replicated = useRef(false);
  const sessionToken = useRef("");
  const refresh = useCallback(async (credentialsOnly = replicated.current) => {
    const request = ++generation.current;
    try {
      if (credentialsOnly) {
        try {
          const session = await api<{ token: string }>("/api/session");
          if (request !== generation.current) return;
          sessionToken.current = session.token;
          setToken(session.token);
          setData((old) =>
            old && old.token !== session.token
              ? { ...old, token: session.token }
              : old,
          );
          setError("");
          return;
        } catch (error) {
          // Keep compatibility with servers that predate the session endpoint.
          if (!(error instanceof ApiError && error.status === 404)) throw error;
        }
      }
      const next = await api<Snapshot>("/api/state");
      if (request !== generation.current) return;
      sessionToken.current = next.token;
      setToken(next.token);
      // Cached projections can arrive before HTTP after a reload. Credentials
      // stay in memory and must reach callers that use explicit request headers.
      setData((old) =>
        replicated.current && old
          ? old.token === next.token
            ? old
            : { ...old, token: next.token }
          : next,
      );
      setError("");
    } catch (e) {
      if (request !== generation.current) return;
      setError(errorText(e));
    }
  }, []);
  useEffect(() => {
    let stopped = false,
      polling = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      if (stopped || polling) return;
      if (document.hidden || navigator.onLine === false) {
        timer = setTimeout(poll, 30000);
        return;
      }
      clearTimeout(timer);
      polling = true;
      try {
        const storage = await syncDatabase();
        if (stopped) return;
        setWorkspaceId(storage.workspaceId);
        await refresh(true);
      } catch (error) {
        if (stopped) return;
        if (error instanceof UnsupportedSyncError) await refresh(false);
        else {
          setError(errorText(error));
          if (!replicated.current) await refresh(false);
        }
      }
      polling = false;
      if (!stopped) timer = setTimeout(poll, replicated.current ? 30000 : 1600);
    };
    const offline = () =>
      setError("Offline. Saved chats and drafts remain on this device.");
    window.addEventListener("offline", offline);
    if (navigator.onLine === false) offline();
    const stopResume = onResume(() => void poll());
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
      stopResume();
      window.removeEventListener("offline", offline);
    };
  }, [refresh]);
  useEffect(
    () =>
      subscribeProjection(
        "state",
        (next) => {
          if (next) {
            replicated.current = true;
            setSyncError("");
            setData({ ...next, token: sessionToken.current });
          }
        },
        (error) => setSyncError(error === null ? "" : errorText(error)),
      ),
    [],
  );
  return { data, error: error || syncError, refresh, workspaceId };
}
// Native input batches have separate user-visible message identities.
export function transcriptMessages(
  source: Message[],
  id: string | null,
): Message[] {
  return source.flatMap((m: Message) =>
    m.inputs
      ? m.inputs.map((r: Json, i: number) => ({
          ...m,
          id: r.id ? `${id}:${r.id}` : `${m.id}:${i}`,
          sourceId: m.id,
          clientMessageId:
            r.clientMessageId ||
            r.id ||
            (i === 0 ? m.clientMessageId : undefined),
          deliveryStatus: r.deliveryStatus,
          deliveryError: r.deliveryError,
          materialized: r.materialized ?? m.materialized,
          pending: r.pending,
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
        }))
      : [m],
  );
}

type TranscriptPage = {
  items: Message[];
  before: string | null;
  after: string | null;
  focused: boolean;
  version?: string;
  latest?: Json;
};

function mergeTranscript(earlier: Message[], later: Message[]): Message[] {
  return [
    ...new Map([...earlier, ...later].map((item) => [item.id, item])).values(),
  ].sort(
    (a, b) => Number(a.at ?? a.created ?? 0) - Number(b.at ?? b.created ?? 0),
  );
}

export function useMessages(
  id: string | null,
  kind: "agent" | "room" | "legacy",
  managed = false,
  workspace = "",
  syncWorkspaceId = "",
) {
  const scope = JSON.stringify([workspace, syncWorkspaceId, kind, managed, id]);
  const history = useRef(
    new Map<
      string,
      { items: Message[]; notice: string; size: number; seq?: number }
    >(),
  );
  const messageSizes = useRef(new WeakMap<Message, number>());
  const pages = useRef(new Map<string, TranscriptPage>());
  const retained =
    managed && kind === "agent" ? history.current.get(scope) : undefined;
  const prefetched =
    managed && kind === "agent" && id && syncWorkspaceId
      ? peekTranscript(syncWorkspaceId, id)
      : undefined;
  const prepared = useMemo(
    () =>
      prefetched
        ? {
            items: transcriptMessages(prefetched.payload?.items || [], id),
            notice:
              prefetched.payload === null
                ? "This session is no longer available."
                : prefetched.payload.unavailable || "",
            seq: prefetched.seq,
          }
        : undefined,
    [prefetched, id],
  );
  const retainedPage = pages.current.get(scope);
  const keepPage =
    retainedPage &&
    (!prefetched ||
      (prefetched.payload &&
        !prefetched.payload.unavailable &&
        (!retainedPage.version ||
          !prefetched.payload.historyVersion ||
          retainedPage.version === prefetched.payload.historyVersion)));
  const cached = keepPage
    ? {
        items: retainedPage!.items,
        notice: retained?.notice || "",
        seq: retained?.seq,
      }
    : prepared && (!retained || prepared.seq > (retained.seq ?? -1))
      ? prepared
      : retained;
  const [items, setItems] = useState<Message[]>([]),
    [loadedId, setLoadedId] = useState<string | null>(null),
    [notice, setNotice] = useState(""),
    [before, setBefore] = useState<number | string | null>(null),
    [after, setAfter] = useState<string | null>(null),
    [historical, setHistorical] = useState(false),
    [pageLoading, setPageLoading] = useState(false),
    [liveAgent, setLiveAgent] = useState<Partial<Agent> | null>(null),
    [connection, setConnection] = useState(""),
    [syncId, setSyncId] = useState<string | null>(null);
  const latest = useRef<{ scope: string; data: Json } | null>(null);
  const pageAttempt = useRef(0);
  const pageBusy = useRef(false);
  const displayed = useRef<{ scope: string; items: Message[] }>({
    scope,
    items,
  });
  displayed.current = {
    scope,
    items: loadedId === scope ? items : cached?.items || [],
  };
  const revision = useRef(0);
  const syncActive = useRef<string | null>(null);
  const active = useRef(scope),
    expanded = useRef(false);
  active.current = scope;
  const accept = useCallback(
    (d: Json, seq?: number) => {
      revision.current++;
      setLoadedId(scope);
      setLiveAgent(d.agent || null);
      const priorItems =
        displayed.current.scope === scope
          ? displayed.current.items
          : history.current.get(scope)?.items || [];
      const liveItems = retainTranscriptItems(
        priorItems,
        transcriptMessages(d.items || [], id),
      );
      let page = pages.current.get(scope);
      if (
        d.unavailable ||
        (page?.version && d.historyVersion && page.version !== d.historyVersion)
      ) {
        pages.current.delete(scope);
        page = undefined;
        setHistorical(false);
      }
      if (page) {
        page.latest = d;
        if (page.focused) {
          const present = new Set(page.items.map((item) => item.id));
          page.items = mergeTranscript(
            page.items,
            liveItems.filter((item) => present.has(item.id)),
          );
        } else {
          // A live snapshot is authoritative for its current time range, including removed queue rows.
          const firstAt = Math.min(
            ...liveItems.map((item) =>
              Number(item.at ?? item.created ?? Infinity),
            ),
          );
          const present = new Set(liveItems.map((item) => item.id));
          page.items = mergeTranscript(
            page.items.filter(
              (item) =>
                present.has(item.id) ||
                Number(item.at ?? item.created ?? 0) < firstAt,
            ),
            liveItems,
          );
        }
      }
      const nextItems = page?.items || liveItems;
      if (managed && kind === "agent") {
        setBefore(
          page
            ? page.before
            : d.nextCursor || (d.truncated ? d.items?.[0]?.id : null),
        );
        setAfter(page?.focused ? page.after : null);
      }
      latest.current = { scope, data: d };
      const nextNotice =
        d.unavailable ||
        (d.truncated && !managed
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
            seq,
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
    [scope, managed, kind, id],
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
    const seed =
      managed && kind === "agent" && id && syncWorkspaceId
        ? peekTranscript(syncWorkspaceId, id)
        : undefined;
    const retained =
      managed && kind === "agent" ? history.current.get(scope) : undefined;
    const page = pages.current.get(scope);
    setLoadedId(retained || page ? scope : null);
    setItems(page?.items || retained?.items || []);
    setHistorical(!!page?.focused);
    setAfter(page?.focused ? page.after : null);
    pageAttempt.current++;
    pageBusy.current = false;
    setPageLoading(false);
    syncActive.current = null;
    setLiveAgent(null);
    setConnection("");
    setNotice(retained?.notice || "");
    setBefore(page?.before || null);
    expanded.current = false;
    if (seed) {
      syncActive.current = scope;
      setSyncId(scope);
      accept(
        seed.payload || {
          items: [],
          unavailable: "This session is no longer available.",
        },
        seed.seq,
      );
      return;
    }
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
    const stopResume = onResume(connect);
    return () => {
      stopped = true;
      clearTimeout(timer);
      source.close();
      window.removeEventListener("offline", offline);
      stopResume();
    };
  }, [load, id, kind, managed, accept, syncId, scope, syncWorkspaceId]);
  useEffect(() => {
    if (!id || kind !== "agent" || !managed) return;
    let seen = false;
    return subscribeProjection(
      `transcript:${id}`,
      (next) => {
        if (active.current !== scope) return;
        if (next) {
          seen = true;
          syncActive.current = scope;
          setSyncId(scope);
          // The shared cache subscription owns transcript updates in this workspace.
          if (!syncWorkspaceId) accept(next);
        } else if (seen && !syncWorkspaceId) {
          accept({
            items: [],
            unavailable: "This session is no longer available.",
          });
        }
      },
      (error) => {
        if (active.current === scope && seen)
          setConnection(error === null ? "live" : "reconnecting");
      },
    );
  }, [id, kind, managed, accept, scope, syncWorkspaceId]);
  useEffect(() => {
    if (!id || !managed || kind !== "agent" || !syncWorkspaceId) return;
    return subscribeTranscript(syncWorkspaceId, id, (entry) => {
      if (active.current !== scope) return;
      syncActive.current = scope;
      setSyncId(scope);
      accept(
        entry.payload || {
          items: [],
          unavailable: "This session is no longer available.",
        },
        entry.seq,
      );
    });
  }, [id, managed, kind, syncWorkspaceId, scope, accept]);
  const fetchPage = async (query: {
    before?: string;
    after?: string;
    around?: string;
  }) => {
    if (
      !id ||
      !managed ||
      kind !== "agent" ||
      (pageBusy.current && !query.around)
    )
      return false;
    const attempt = ++pageAttempt.current;
    pageBusy.current = true;
    setPageLoading(true);
    try {
      const params = new URLSearchParams({ id, ...query });
      const result = await api(`/api/transcript/page?${params}`);
      if (active.current !== scope || attempt !== pageAttempt.current)
        return false;
      const source = transcriptMessages(result.items || [], id);
      const prior = pages.current.get(scope);
      const live =
        latest.current?.scope === scope ? latest.current.data : undefined;
      if (
        result.historyVersion &&
        live?.historyVersion &&
        result.historyVersion !== live.historyVersion
      )
        throw new Error(
          "The conversation changed. Search or load its history again.",
        );
      const existing =
        displayed.current.scope === scope ? displayed.current.items : [];
      const focused = !!query.around || !!prior?.focused;
      const next: TranscriptPage = {
        items: query.around
          ? source
          : query.after
            ? mergeTranscript(existing, source)
            : mergeTranscript(source, existing),
        before: query.after ? prior?.before || null : result.nextCursor || null,
        after: focused
          ? query.before
            ? prior?.after || null
            : result.nextAfterCursor || null
          : null,
        focused,
        version: result.historyVersion,
        latest: live,
      };
      pages.current.delete(scope);
      pages.current.set(scope, next);
      while (pages.current.size > 12)
        pages.current.delete(pages.current.keys().next().value!);
      setItems(next.items);
      setBefore(next.before);
      setAfter(next.after);
      setHistorical(next.focused);
      setLoadedId(scope);
      return true;
    } finally {
      if (attempt === pageAttempt.current) {
        pageBusy.current = false;
        setPageLoading(false);
      }
    }
  };
  const showLatest = () => {
    pageAttempt.current++;
    pageBusy.current = false;
    setPageLoading(false);
    if (!pages.current.get(scope)?.focused) return;
    const data =
      pages.current.get(scope)?.latest ||
      (latest.current?.scope === scope ? latest.current.data : undefined);
    pages.current.delete(scope);
    setHistorical(false);
    setAfter(null);
    if (data) accept(data);
  };
  const ensureMessage = async (messageId: string) => {
    const present =
      displayed.current.scope === scope &&
      displayed.current.items.some(
        (item) => item.id === messageId || item.sourceId === messageId,
      );
    if (present) return true;
    if (!managed || kind !== "agent") return false;
    return fetchPage({ around: messageId });
  };
  const newer = async () => {
    if (after) await fetchPage({ after });
  };
  const older = async () => {
    if (!id || !before) return;
    if (managed && kind === "agent") {
      await fetchPage({ before: String(before) });
      return;
    }
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
    before:
      loadedId === scope
        ? before
        : keepPage
          ? retainedPage!.before
          : prefetched?.payload?.nextCursor || null,
    older,
    newer,
    after:
      loadedId === scope
        ? after
        : keepPage && retainedPage!.focused
          ? retainedPage!.after
          : null,
    historical:
      loadedId === scope ? historical : !!(keepPage && retainedPage!.focused),
    pageLoading,
    showLatest,
    ensureMessage,
    reload: load,
    liveAgent: loadedId === scope ? liveAgent : null,
    connection: loadedId === scope ? connection : "",
  };
}
