import { retainTranscriptItems } from "./conversation/transcriptIdentity";
import {
  boundTranscriptItems,
  trimTranscriptPageCache,
} from "./transcriptPageBounds";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  syncGet,
  ApiError,
  errorText,
  refreshSession,
  saved,
  save,
  type PostResult,
} from "./api";
import {
  refreshProjection,
  subscribeProjection,
  watchResourceConnection,
  syncDatabase,
  watchResourceChanges,
} from "./sync/client";
import { peekTranscript, subscribeTranscript } from "./sync/transcriptCache";
import { onResume } from "./sync/resume";
import { agentChatMessages } from "./hooks/agentChatMessages";
import { drainRoomUpdates } from "./hooks/roomUpdates";
import { snapshotAgentFromMutation } from "./hooks/snapshotAgentFromMutation";
import type { GetResult } from "./api";
import type { Message, Agent, Json } from "./types";
type StateSnapshot = GetResult<"/api/state">;
type TranscriptPageData = GetResult<"/api/transcript">;
export function useSnapshot() {
  const [data, setData] = useState<StateSnapshot | null>(null),
    [error, setError] = useState("");
  const [syncError, setSyncError] = useState("");
  const [transportError, setTransportError] = useState("");
  const [workspaceId, setWorkspaceId] = useState("");
  const [created, setCreated] = useState<{ scope: string; agents: Agent[] }>(
    () => {
      const cached = saved<{ scope: string; agents: Agent[] } | null>(
        "codex-confirmed-chats",
        null,
      );
      return cached &&
        typeof cached.scope === "string" &&
        Array.isArray(cached.agents)
        ? cached
        : { scope: "", agents: [] };
    },
  );
  // Match the durable creation request's workspace scope, including HTTP fallback.
  const scope = data?.stateDir || "";
  const currentScope = useRef(scope);
  currentScope.current = scope;
  const rememberCreated = useCallback(
    (agent: PostResult<"/api/leads">, expectedScope: string) => {
      if (currentScope.current !== expectedScope)
        throw new Error("The workspace changed before the new chat opened.");
      // A fresh draft has no native thread. Existing-thread retries await its projection.
      const confirmed = snapshotAgentFromMutation(agent);
      setCreated((old) => ({
        scope: expectedScope,
        agents: [
          ...(old.scope === expectedScope ? old.agents : []).filter(
            (a) => a.id !== agent.id,
          ),
          confirmed,
        ],
      }));
    },
    [],
  );
  const forgetCreated = useCallback((ids: string[]) => {
    setCreated((old) => ({
      ...old,
      agents: old.agents.filter((a) => !ids.includes(a.id)),
    }));
  }, []);
  useEffect(() => {
    if (!data) return;
    setCreated((old) => {
      if (old.scope !== scope)
        return old.agents.length ? { scope, agents: [] } : old;
      const remaining = old.agents.filter(
        (a) => !data?.threads.some((row) => row.id === a.id),
      );
      return remaining.length === old.agents.length
        ? old
        : { scope, agents: remaining };
    });
  }, [data, scope, created]);
  useEffect(() => {
    save("codex-confirmed-chats", created);
  }, [created]);
  const visibleData = useMemo(() => {
    if (!data || created.scope !== scope || !created.agents.length) return data;
    const missing = created.agents.filter(
      (a) => !data.threads.some((row) => row.id === a.id),
    );
    if (!missing.length) return data;
    const runtime = data.runtime;
    return {
      ...data,
      threads: [...data.threads, ...missing],
      ...(runtime
        ? {
            runtime: {
              ...runtime,
              agents: [
                ...runtime.agents.filter(
                  (a) => !missing.some((row) => row.id === a.id),
                ),
                ...missing,
              ],
            },
          }
        : {}),
    };
  }, [data, created, scope]);
  const generation = useRef(0);
  const replicated = useRef(false);
  const sessionToken = useRef("");
  const legacyFallback = useRef(false);
  const credentialRetry = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  const credentialFailures = useRef(0);
  const refresh = useCallback(
    async (credentialsOnly = replicated.current): Promise<void> => {
      clearTimeout(credentialRetry.current);
      credentialRetry.current = undefined;
      const request = ++generation.current;
      try {
        const session = await refreshSession();
        if (request !== generation.current) return;
        sessionToken.current = session.token;
        setData((old) =>
          old && old.token !== session.token
            ? { ...old, token: session.token }
            : old,
        );
        if (!credentialsOnly) {
          try {
            await refreshProjection("state");
          } catch (projectionError) {
            // A renderer can update before the server patch. Keep the previous
            // snapshot route as a first-load fallback until entity sync exists.
            if (replicated.current) throw projectionError;
            const legacy = await syncGet("/api/state", {
              query: { view: "chat" },
            });
            if (request !== generation.current) return;
            setData({ ...legacy, token: session.token });
          }
        }
        if (request !== generation.current) return;
        credentialFailures.current = 0;
        setError("");
      } catch (e) {
        if (request !== generation.current) return;
        setError(errorText(e));
        // Retry this read without waiting for the healthy 30-second poll. Writes
        // retain their own receipt rules and never use this retry.
        const transient =
          e instanceof TypeError ||
          (e instanceof ApiError &&
            ((e.status >= 500 && e.status < 600) ||
              [408, 429].includes(e.status)));
        if (transient && !document.hidden && navigator.onLine !== false) {
          const delay = Math.min(
            500 * 2 ** Math.min(credentialFailures.current++, 4),
            8000,
          );
          credentialRetry.current = setTimeout(() => {
            credentialRetry.current = undefined;
            if (
              request === generation.current &&
              !document.hidden &&
              navigator.onLine !== false
            )
              void refresh(credentialsOnly);
          }, delay);
        }
      }
    },
    [],
  );
  useEffect(() => {
    let stopped = false;
    const initialize = async () => {
      try {
        const storage = await syncDatabase();
        if (stopped) return;
        setWorkspaceId(storage.workspaceId);
        await refresh(true);
      } catch (error) {
        if (stopped) return;
        setError(errorText(error));
        if (!replicated.current) await refresh(true);
      }
    };
    const offline = () =>
      setError("Offline. Your chats and drafts are saved here.");
    window.addEventListener("offline", offline);
    if (navigator.onLine === false) offline();
    const stopResume = onResume(() => void refresh(true));
    void initialize();
    return () => {
      stopped = true;
      generation.current++;
      clearTimeout(credentialRetry.current);
      credentialRetry.current = undefined;
      stopResume();
      window.removeEventListener("offline", offline);
    };
  }, [refresh]);
  useEffect(
    () =>
      watchResourceConnection((status) => {
        setTransportError(
          status === "degraded"
            ? "Live updates are reconnecting."
            : status === "offline"
              ? "Offline. Live updates will resume when connected."
              : "",
        );
      }),
    [],
  );
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
        (error) => {
          setSyncError(error === null ? "" : errorText(error));
          if (
            error !== null &&
            !replicated.current &&
            !legacyFallback.current
          ) {
            legacyFallback.current = true;
            void syncGet("/api/state", { query: { view: "chat" } })
              .then((legacy) => {
                sessionToken.current = legacy.token || sessionToken.current;
                setData({ ...legacy, token: sessionToken.current });
              })
              .catch((fallbackError) => setError(errorText(fallbackError)));
          }
        },
      ),
    [],
  );
  return {
    data: visibleData,
    error: error || syncError || transportError,
    refresh,
    workspaceId,
    rememberCreated,
    forgetCreated,
    creationScope: scope,
  };
}
// Name the sender of team events, so a list of events is not identical rows.
function eventSender(input: Json): string {
  if (input.kind !== "agent_message" && input.kind !== "child_result")
    return "";
  try {
    const event = JSON.parse(String(input.text || ""));
    const name = event?.sender_name || event?.name;
    if (typeof name !== "string" || !name.trim()) return "";
    return input.kind === "child_result"
      ? `Result from ${name.trim()}`
      : `Message from ${name.trim()}`;
  } catch {
    return "";
  }
}

// Native input batches have separate user-visible message identities.
export function transcriptMessages(
  source: Message[],
  id: string | null,
): Message[] {
  return source.flatMap((m: Message) =>
    m.inputs
      ? m.inputs.map((r, i) => ({
          ...m,
          id: r.id ? `${id}:${r.id}` : `${m.id}:${i}`,
          sourceId: m.id,
          clientMessageId:
            r.clientMessageId ||
            r.id ||
            (i === 0 ? m.clientMessageId : undefined),
          deliveryStatus: r.deliveryStatus,
          requestedDelivery: r.requestedDelivery,
          deliveryError: r.deliveryError,
          materialized: r.materialized ?? m.materialized,
          pending: r.pending ?? undefined,
          assets: r.assets || (i === 0 ? m.assets : []),
          role: r.kind === "user" ? "user" : "tool",
          text: r.text,
          truncated: r.truncated,
          title:
            r.kind === "user"
              ? "You"
              : eventSender(r) ||
                (
                  {
                    agent_message: "Agent message received",
                    child_result: "Worker result received",
                    monitor_exit: "Command finished",
                    monitor_cancelled: "Command cancelled",
                    complaint_response: "Complaint response received",
                    followup: "Agent follow-up",
                    complaint: "Complaint requires a response",
                    work_released: "Task released from a failed worker",
                  } as Record<string, string>
                )[r.kind] ||
                "Team activity",
        }))
      : [m],
  );
}

type TranscriptPage = {
  items: Message[];
  size: number;
  anchorId?: string;
  before: string | null;
  after: string | null;
  focused: boolean;
  version?: string;
  latest?: TranscriptPageData;
};

type RoomPage = {
  items: Message[];
  size: number;
  before: number | null;
  after: number | null;
  pollAfter: number | null;
  anchorId?: string;
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
  const roomPages = useRef(new Map<string, RoomPage>());
  const sizeOfMessage = (item: Message) => {
    let size = messageSizes.current.get(item);
    if (size === undefined) {
      size = JSON.stringify(item).length * 2;
      messageSizes.current.set(item, size);
    }
    return size;
  };
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
    [liveAgent, setLiveAgent] = useState<TranscriptPageData["agent"] | null>(
      null,
    ),
    [connection, setConnection] = useState(""),
    [syncId, setSyncId] = useState<string | null>(null);
  const latest = useRef<{ scope: string; data: TranscriptPageData } | null>(
    null,
  );
  const pageAttempt = useRef(0);
  const pageBusy = useRef(false);
  const pageAnchor = useRef<{ scope: string; id: string } | null>(null);
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
    (d: TranscriptPageData, seq?: number) => {
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
        } else if (!d.truncated) {
          page.items = liveItems;
          page.before = null;
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
                (!item.pending &&
                  item.materialized !== false &&
                  Number(item.at ?? item.created ?? 0) < firstAt),
            ),
            liveItems,
          );
        }
        const bounded = boundTranscriptItems(
          page.items,
          (item) => {
            let size = messageSizes.current.get(item);
            if (size === undefined) {
              size = JSON.stringify(item).length * 2;
              messageSizes.current.set(item, size);
            }
            return size;
          },
          page.focused ? "oldest" : "newest",
          // A live page always keeps the newest messages; only a focused
          // history page may trim them around its scroll anchor.
          page.focused ? page.anchorId : undefined,
        );
        page.items = bounded.items;
        page.size = bounded.bytes;
        if (bounded.droppedOldest)
          page.before = bounded.items[0]?.id || page.before;
        if (bounded.droppedNewest && page.focused)
          page.after = page.items.at(-1)?.id || page.after;
        if (!page.focused) page.after = null;
      }
      if (!page && !d.unavailable && managed && kind === "agent") {
        // Retain the loaded range from the first snapshot. Otherwise each live
        // update evicts messages as tool activity moves the server's page.
        const bounded = boundTranscriptItems(liveItems, (item) => {
          let size = messageSizes.current.get(item);
          if (size === undefined) {
            size = JSON.stringify(item).length * 2;
            messageSizes.current.set(item, size);
          }
          return size;
        });
        page = {
          items: bounded.items,
          size: bounded.bytes,
          before: bounded.droppedOldest
            ? bounded.items[0]?.id || null
            : d.nextCursor || (d.truncated ? d.items?.[0]?.id : null),
          after: null,
          focused: false,
          version: d.historyVersion ?? undefined,
          latest: d,
        };
        pages.current.set(scope, page);
      }
      if (page) {
        pages.current.delete(scope);
        pages.current.set(scope, page);
        trimTranscriptPageCache(pages.current, scope);
      }
      const nextItems = page?.items || liveItems;
      if (managed && kind === "agent") {
        setBefore(
          page
            ? page.before
            : d.nextCursor || (d.truncated ? d.items?.[0]?.id : null),
        );
        setAfter(page?.after || null);
      }
      latest.current = { scope, data: d };
      const nextNotice =
        d.unavailable ||
        (d.truncated && !managed ? "Recent messages only." : "");
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
          const page = roomPages.current.get(scope);
          const cursor = page?.pollAfter ?? null;
          if (page && cursor !== null) {
            const result = await drainRoomUpdates(
              page.items,
              cursor,
              (after) =>
                syncGet("/api/agent-chat", {
                  query: { room: id, after, limit: 100 },
                }),
              current,
            );
            if (!current() || !result || result.checkpoint === cursor) return;
            const bounded = boundTranscriptItems(
              result.items,
              sizeOfMessage,
              "newest",
              page.anchorId,
            );
            const next: RoomPage = {
              items: bounded.items,
              size: bounded.bytes,
              before: bounded.droppedOldest
                ? (bounded.items[0]?.seq ?? page.before)
                : page.before,
              after: bounded.droppedNewest
                ? (bounded.items.at(-1)?.seq ?? page.after)
                : page.after,
              pollAfter: result.checkpoint,
              anchorId: page.anchorId,
            };
            roomPages.current.set(scope, next);
            trimTranscriptPageCache(roomPages.current, scope);
            setLoadedId(scope);
            setBefore(next.before);
            setAfter(next.after == null ? null : String(next.after));
            setItems(next.items);
            return;
          }
          const d = await syncGet("/api/agent-chat", {
            query: {
              room: id,
              limit: 100,
              ...(cursor != null ? { after: cursor } : {}),
            },
          });
          if (!current()) return;
          setNotice("");
          setLoadedId(scope);
          const received = agentChatMessages(d.messages);
          if (!page || cursor == null) {
            const bounded = boundTranscriptItems(received, sizeOfMessage);
            const next: RoomPage = {
              items: bounded.items,
              size: bounded.bytes,
              before: d.nextBefore ?? null,
              after: null,
              pollAfter: received.at(-1)?.seq ?? null,
            };
            roomPages.current.set(scope, next);
            trimTranscriptPageCache(roomPages.current, scope);
            setBefore(next.before);
            setAfter(null);
            setItems(next.items);
          } else if (received.length) {
            const bounded = boundTranscriptItems(
              [...page.items, ...received],
              sizeOfMessage,
              "newest",
              page.anchorId,
            );
            const next: RoomPage = {
              items: bounded.items,
              size: bounded.bytes,
              before: bounded.droppedOldest
                ? (bounded.items[0]?.seq ?? page.before)
                : page.before,
              after: bounded.droppedNewest
                ? (bounded.items.at(-1)?.seq ?? page.after)
                : page.after,
              pollAfter: received.at(-1)?.seq ?? page.pollAfter,
              anchorId: page.anchorId,
            };
            roomPages.current.set(scope, next);
            trimTranscriptPageCache(roomPages.current, scope);
            setBefore(next.before);
            setAfter(next.after == null ? null : String(next.after));
            setItems(next.items);
          }
        } else if (kind === "legacy") {
          const d = await syncGet("/api/messages", { query: { room: id } });
          if (!current()) return;
          setNotice("");
          setLoadedId(scope);
          setItems(
            d.map((m) => ({
              id: m.id,
              text: m.text,
              at: m.at,
              role: m.author === "user" ? "user" : "assistant",
              deliveryStatus: m.status ?? undefined,
              deliveryError: m.error ?? undefined,
            })),
          );
        } else {
          const d = await syncGet("/api/transcript", { query: { id } });
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
    const roomPage = kind === "room" ? roomPages.current.get(scope) : undefined;
    setLoadedId(retained || page || roomPage ? scope : null);
    setItems(page?.items || retained?.items || roomPage?.items || []);
    setHistorical(!!page?.focused);
    setAfter(page?.after || roomPage?.after?.toString() || null);
    pageAttempt.current++;
    pageBusy.current = false;
    setPageLoading(false);
    syncActive.current = null;
    setLiveAgent(null);
    setConnection("");
    setNotice(retained?.notice || "");
    setBefore(page?.before || roomPage?.before || null);
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
    if (!id) return;
    if (managed && kind === "agent" && syncWorkspaceId) return;
    const resource =
      kind === "room"
        ? ({ kind: "room", roomId: id } as const)
        : ({ kind: "transcript", agentId: id } as const);
    const stopWatch = watchResourceChanges(
      resource,
      () => void load(() => !stopped),
    );
    return () => {
      stopped = true;
      stopWatch();
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
            truncated: false,
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
    anchor?: string;
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
      const result = await syncGet("/api/transcript/page", {
        query: {
          id,
          ...(query.before ? { before: query.before } : {}),
          ...(query.after ? { after: query.after } : {}),
          ...(query.around ? { around: query.around } : {}),
        },
      });
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
        size: 0,
        before: query.after ? prior?.before || null : result.nextCursor || null,
        after: query.before
          ? result.nextAfterCursor || prior?.after || null
          : result.nextAfterCursor || null,
        focused,
        anchorId: query.anchor || prior?.anchorId || query.around,
        version: result.historyVersion ?? undefined,
        latest: live,
      };
      const bounded = boundTranscriptItems(
        next.items,
        (item) => {
          let size = messageSizes.current.get(item);
          if (size === undefined) {
            size = JSON.stringify(item).length * 2;
            messageSizes.current.set(item, size);
          }
          return size;
        },
        query.before ? "oldest" : "newest",
        next.anchorId,
      );
      next.items = bounded.items;
      next.size = bounded.bytes;
      if (bounded.droppedOldest)
        next.before = bounded.items[0]?.id || next.before;
      if (bounded.droppedNewest && result.nextAfterCursor)
        next.after = result.nextAfterCursor;
      pages.current.delete(scope);
      pages.current.set(scope, next);
      trimTranscriptPageCache(pages.current, scope);
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
    if (kind === "room") {
      roomPages.current.delete(scope);
      setItems([]);
      setBefore(null);
      setAfter(null);
      setLoadedId(null);
      void load();
      return;
    }
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
    return fetchPage({ around: messageId, anchor: messageId });
  };
  const setPageAnchor = (anchorId: string | null) => {
    pageAnchor.current = anchorId ? { scope, id: anchorId } : null;
    const page = pages.current.get(scope);
    if (page) page.anchorId = anchorId || undefined;
    const roomPage = roomPages.current.get(scope);
    if (roomPage) roomPage.anchorId = anchorId || undefined;
  };
  const newer = async () => {
    if (kind === "room" && id) {
      const page = roomPages.current.get(scope);
      if (!page?.after) return;
      const d = await syncGet("/api/agent-chat", {
        query: { room: id, after: page.after, limit: 100 },
      });
      if (active.current !== scope) return;
      const later = agentChatMessages(d.messages);
      const bounded = boundTranscriptItems(
        [...page.items, ...later],
        sizeOfMessage,
        "newest",
        page.anchorId,
      );
      const next: RoomPage = {
        items: bounded.items,
        size: bounded.bytes,
        before: bounded.droppedOldest
          ? (bounded.items[0]?.seq ?? page.before)
          : page.before,
        after: bounded.droppedNewest
          ? (bounded.items.at(-1)?.seq ?? page.after)
          : (d.nextAfter ?? null),
        pollAfter: later.at(-1)?.seq ?? page.pollAfter,
        anchorId: page.anchorId,
      };
      roomPages.current.set(scope, next);
      setItems(next.items);
      setBefore(next.before);
      setAfter(next.after == null ? null : String(next.after));
      return;
    }
    if (after)
      await fetchPage({
        after,
        anchor:
          pageAnchor.current?.scope === scope
            ? pageAnchor.current.id
            : undefined,
      });
  };
  const older = async () => {
    if (!id || !before) return;
    if (kind === "room") {
      const page = roomPages.current.get(scope);
      if (!page) return;
      const cursor =
        page.before ?? (typeof before === "number" ? before : Number(before));
      if (!Number.isSafeInteger(cursor) || cursor < 0) return;
      const d = await syncGet("/api/agent-chat", {
        query: {
          room: id,
          before: cursor,
          limit: 100,
        },
      });
      if (!d) return;
      if (active.current !== scope) return;
      const olderItems = agentChatMessages(d.messages);
      const bounded = boundTranscriptItems(
        [...olderItems, ...page.items],
        sizeOfMessage,
        "oldest",
        page.anchorId,
      );
      const next: RoomPage = {
        items: bounded.items,
        size: bounded.bytes,
        before: bounded.droppedOldest
          ? (bounded.items[0]?.seq ?? d.nextBefore ?? null)
          : (d.nextBefore ?? null),
        after: bounded.droppedNewest
          ? (bounded.items.at(-1)?.seq ?? page.after)
          : null,
        pollAfter: page.pollAfter,
        anchorId: page.anchorId,
      };
      roomPages.current.set(scope, next);
      trimTranscriptPageCache(roomPages.current, scope);
      expanded.current = true;
      setBefore(next.before);
      setAfter(next.after == null ? null : String(next.after));
      setItems(next.items);
      return;
    }
    if (managed && kind === "agent") {
      await fetchPage({
        before: String(before),
        anchor:
          pageAnchor.current?.scope === scope
            ? pageAnchor.current.id
            : undefined,
      });
      return;
    }
    const cursor = typeof before === "number" ? before : Number(before);
    if (!Number.isSafeInteger(cursor) || cursor < 0) return;
    const d = await syncGet("/api/agent-chat", {
      query: { room: id, before: cursor },
    });
    if (!d) return;
    if (active.current !== scope) return;
    expanded.current = true;
    setBefore(d.nextBefore ?? null);
    setItems((old) =>
      [
        ...new Map(
          [...agentChatMessages(d.messages), ...old].map((m) => [m.id, m]),
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
    historyVersion:
      pages.current.get(scope)?.version ||
      prefetched?.payload?.historyVersion ||
      "",
    older,
    newer,
    setPageAnchor,
    after: loadedId === scope ? after : keepPage ? retainedPage!.after : null,
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
