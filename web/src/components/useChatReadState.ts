import {
  useCallback,
  useMemo,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type RefObject,
} from "react";
import { api, ApiError, errorText } from "../api";
import type { Agent, Message, Snapshot } from "../types";

export type ChatReadState = {
  threadId: string;
  turnId: string;
  read: boolean;
  revision: number;
};
export type ChatReadProof = { id: string; threadId: string; turnId: string };
const completed = (agent: Agent): ChatReadProof | null =>
  agent.threadId &&
  agent.lastCompletedTurn &&
  agent.lastCompletedTurnStatus === "completed"
    ? {
        id: agent.id,
        threadId: agent.threadId,
        turnId: agent.lastCompletedTurn,
      }
    : null;
const sameResult = (a: ChatReadProof, b: ChatReadProof) =>
  a.id === b.id && a.threadId === b.threadId && a.turnId === b.turnId;
const storedState = (agent: Agent): ChatReadState | null => {
  const value = agent.readState;
  return value &&
    typeof value.threadId === "string" &&
    typeof value.turnId === "string" &&
    typeof value.read === "boolean" &&
    Number.isSafeInteger(value.revision) &&
    value.revision >= 0
    ? value
    : null;
};

export function useChatReadState(
  data: Snapshot | null,
  opened: string | null,
  notify: (message: string) => void,
  refresh: () => Promise<void>,
  workspaceId?: string,
) {
  const [, redraw] = useState(0);
  const [metadataVersion, changedMetadata] = useState(0);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const key = JSON.stringify([data?.stateDir || "", workspaceId || ""]);
  const scope = useRef({
    key,
    states: new Map<string, ChatReadState>(),
    requests: new Map<string, Promise<void>>(),
    suppressed: new Set<string>(),
    attempted: new Set<string>(),
    uncertain: new Set<string>(),
    opened,
  });
  if (scope.current.key !== key)
    scope.current = {
      key,
      states: new Map(),
      requests: new Map(),
      suppressed: new Set(),
      attempted: new Set(),
      uncertain: new Set(),
      opened,
    };
  if (scope.current.opened !== opened) {
    scope.current.suppressed.clear();
    scope.current.attempted.clear();
    scope.current.opened = opened;
  }
  const latest = useRef({ data, opened, notify, refresh, workspaceId });
  latest.current = { data, opened, notify, refresh, workspaceId };
  const readStateFor = useCallback(
    (agent: Agent): ChatReadState | null => {
      const saved = storedState(agent);
      const known = scope.current.states.get(agent.id);
      if (saved && (!known || saved.revision > known.revision)) {
        scope.current.states.set(agent.id, saved);
        return saved;
      }
      return known || saved;
    },
    [key, metadataVersion],
  );
  const write = async (proof: ChatReadProof, read: boolean) => {
    const current = scope.current;
    const valid = () => mounted.current && scope.current === current;
    const pending = current.requests.get(proof.id);
    if (pending) {
      if (read) return;
      await pending;
      if (!valid()) return;
      return write(proof, read);
    }
    const agent = latest.current.data?.threads.find(
      (value) => value.id === proof.id,
    );
    const result = agent && completed(agent);
    if (
      !agent ||
      agent.readStateSupported !== true ||
      !result ||
      !sameResult(result, proof)
    )
      return;
    if (
      read &&
      (latest.current.opened !== proof.id ||
        current.suppressed.has(proof.id) ||
        document.visibilityState !== "visible" ||
        !document.hasFocus())
    )
      return;
    const saved = readStateFor(agent);
    if (
      saved?.threadId === proof.threadId &&
      saved.turnId === proof.turnId &&
      saved.read === read &&
      !current.uncertain.has(proof.id)
    )
      return;
    const attempt = JSON.stringify([
      proof.id,
      proof.threadId,
      proof.turnId,
      saved?.revision || 0,
    ]);
    if (read && current.attempted.has(attempt)) return;
    if (read) current.attempted.add(attempt);
    const request = (async () => {
      try {
        const body = {
          id: proof.id,
          read_state: {
            thread_id: proof.threadId,
            turn_id: proof.turnId,
            read,
            expected_revision: saved?.revision || 0,
          },
        };
        const requestWorkspace = latest.current.workspaceId;
        let canonical: Agent;
        for (let retry = 0; ; retry++) {
          try {
            canonical = await api<Agent>("/api/organization", body, {
              workspaceId: requestWorkspace,
              timeoutMs: 15000,
            });
            break;
          } catch (error) {
            const transient =
              error instanceof TypeError ||
              (error instanceof ApiError &&
                (error.status >= 500 || [408, 429].includes(error.status)));
            if (!read || !transient || retry >= 2) throw error;
            current.uncertain.add(proof.id);
            await new Promise((resolve) =>
              setTimeout(resolve, 1000 * (retry + 1)),
            );
            // Repeat the same revision. A newer Unread must win over this retry.
            if (!valid()) return;
            if (latest.current.opened !== proof.id) throw error;
          }
        }
        if (!valid()) return;
        const state = storedState(canonical);
        if (
          canonical.id !== proof.id ||
          !state ||
          state.threadId !== proof.threadId ||
          state.turnId !== proof.turnId ||
          state.read !== read ||
          state.revision <= (saved?.revision || 0)
        )
          throw new Error("The server did not confirm the read status.");
        const known = current.states.get(proof.id);
        if (!known || state.revision >= known.revision)
          current.states.set(proof.id, state);
        current.uncertain.delete(proof.id);
        changedMetadata((value) => value + 1);
        redraw((value) => value + 1);
        await latest.current.refresh();
      } catch (error) {
        if (valid()) {
          current.attempted.delete(attempt);
          current.uncertain.add(proof.id);
          // Reconcile a lost success before a queued Unread can use local state.
          // The app's normal refresh can update only credentials during sync.
          try {
            const snapshot = await api<Snapshot>("/api/state?view=chat");
            if (!valid()) return;
            const canonical = snapshot.threads.find(
              (value) => value.id === proof.id,
            );
            const result = canonical && completed(canonical);
            if (
              snapshot.stateDir === latest.current.data?.stateDir &&
              result &&
              sameResult(result, proof)
            ) {
              const state = storedState(canonical!);
              const known = current.states.get(proof.id);
              if (state && (!known || state.revision >= known.revision))
                current.states.set(proof.id, state);
              if (state || !known) current.uncertain.delete(proof.id);
              changedMetadata((value) => value + 1);
            }
          } catch {
            // Keep uncertainty visible; a local no-op cannot confirm this write.
          }
          if (valid()) latest.current.notify(errorText(error));
        }
      }
    })();
    current.requests.set(proof.id, request);
    redraw((value) => value + 1);
    try {
      await request;
    } finally {
      if (current.requests.get(proof.id) === request)
        current.requests.delete(proof.id);
      if (valid()) redraw((value) => value + 1);
    }
  };
  const writeCurrent = useRef(write);
  writeCurrent.current = write;
  const markUnread = useCallback(async (agent: Agent) => {
    const proof = completed(agent);
    if (!proof || agent.readStateSupported !== true) return;
    if (latest.current.opened === agent.id)
      scope.current.suppressed.add(agent.id);
    await writeCurrent.current(proof, false);
  }, []);
  const observeRead = useCallback((proof: ChatReadProof) => {
    void writeCurrent.current(proof, true);
  }, []);
  return {
    readStateFor,
    marking: new Set(scope.current.requests.keys()),
    markUnread,
    observeRead,
  };
}

// Transcript selection alone is not a read. Observe the final result in its scroll root.
export function useVisibleChatResult(
  scroll: RefObject<HTMLDivElement | null>,
  agent: Agent | undefined,
  items: Message[],
  loaded: boolean,
  onReadResult?: (proof: ChatReadProof) => void,
  workspace?: string,
  latestPage = false,
) {
  const proof = agent && completed(agent);
  const finalId = useMemo(() => {
    const candidates = proof
      ? items.filter(
          (item) =>
            item.role === "assistant" &&
            item.turnId === proof.turnId &&
            (!item.phase || item.phase === "final_answer") &&
            !item.pending &&
            !item.streaming &&
            !!item.text?.trim(),
        )
      : [];
    return (
      candidates.filter((item) => item.phase === "final_answer").at(-1) ||
      candidates.at(-1)
    )?.id;
  }, [items, proof?.turnId]);
  const completedTurnVisible =
    latestPage &&
    items.some(
      (item) =>
        item.turnId === proof?.turnId &&
        item.turnStatus === "completed" &&
        !item.pending &&
        !item.streaming,
    );
  const callback = useRef(onReadResult);
  useLayoutEffect(() => {
    callback.current = onReadResult;
  });
  useLayoutEffect(() => {
    if (
      !loaded ||
      !agent ||
      agent.readStateSupported !== true ||
      !callback.current ||
      !proof ||
      (!finalId && !completedTurnVisible)
    )
      return;
    const root = scroll.current;
    if (!root) return;
    let target: HTMLElement | undefined;
    let disposed = false;
    let reported = false;
    const visible = () => {
      if (
        disposed ||
        reported ||
        document.visibilityState !== "visible" ||
        !document.hasFocus() ||
        !target?.isConnected ||
        target.hasAttribute("data-lazy-message") ||
        (!finalId && target.dataset.outcome !== "completed")
      )
        return;
      const box = target.getBoundingClientRect();
      const bounds = root.getBoundingClientRect();
      if (
        Math.min(box.bottom, bounds.bottom, window.innerHeight) >
          Math.max(box.top, bounds.top, 0) &&
        Math.min(box.right, bounds.right, window.innerWidth) >
          Math.max(box.left, bounds.left, 0)
      ) {
        reported = true;
        callback.current?.(proof);
      }
    };
    const observer = new IntersectionObserver(
      (entries) => {
        if (
          entries.some(
            (entry) => entry.target === target && !entry.isIntersecting,
          )
        )
          reported = false;
        if (
          entries.some(
            (entry) => entry.target === target && entry.isIntersecting,
          )
        )
          visible();
      },
      { root },
    );
    const connect = () => {
      if (target?.isConnected) return;
      const next =
        root.querySelector<HTMLElement>(
          finalId
            ? `[data-message="${CSS.escape(finalId)}"]:not([data-lazy-message])`
            : `[data-turn="${CSS.escape(proof.turnId)}"][data-outcome="completed"]`,
        ) || undefined;
      if (target !== next) {
        observer.disconnect();
        target = next;
        reported = false;
        if (target) observer.observe(target);
      }
    };
    const resume = () => {
      reported = false;
      connect();
      visible();
    };
    const mutations = new MutationObserver((records) => {
      connect();
      if (
        records.some(
          (record) => record.type === "attributes" && record.target === target,
        )
      )
        visible();
    });
    mutations.observe(root, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["data-outcome"],
    });
    connect();
    window.addEventListener("focus", resume);
    window.addEventListener("online", resume);
    document.addEventListener("visibilitychange", resume);
    return () => {
      disposed = true;
      observer.disconnect();
      mutations.disconnect();
      window.removeEventListener("focus", resume);
      window.removeEventListener("online", resume);
      document.removeEventListener("visibilitychange", resume);
    };
  }, [
    scroll,
    proof?.id,
    proof?.threadId,
    proof?.turnId,
    agent?.readStateSupported,
    finalId,
    completedTurnVisible,
    loaded,
    !!onReadResult,
    workspace,
  ]);
}
