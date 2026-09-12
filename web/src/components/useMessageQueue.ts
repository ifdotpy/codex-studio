import { useCallback, useEffect, useRef, useState } from "react";
import { api, ApiError, errorText, saved } from "../api";
import { updateLocalDraft } from "../sync/localDraft";
import { onResume } from "../sync/resume";
import type { Json } from "../types";
import type { QueueItem } from "./MessageQueue";

type QueueView = {
  items: QueueItem[];
  revision?: string;
  capabilities?: { reorder?: boolean; receipts?: boolean };
};

export function useMessageQueue(p: {
  id: string | null;
  enabled: boolean;
  scope: string;
  workspaceId?: string;
  observed?: (ids: string[]) => void;
  edited?: (id: string, text: string) => void;
  refresh: () => Promise<void>;
}) {
  const key = `studio-queue-change:${p.scope}:${p.id}`;
  const currentKey = useRef(key);
  currentKey.current = key;
  const [state, setState] = useState<{
    key: string;
    view: QueueView;
    loaded?: boolean;
  }>({
    key,
    view: { items: [] },
  });
  const [failure, setFailure] = useState({ key, text: "" });
  const [pendingState, setPending] = useState<{
    key: string;
    request: Json | null;
  }>({
    key,
    request: saved<Json | null>(key, null),
  });
  const [busyKey, setBusy] = useState<string | null>(null);
  const locks = useRef(new Set<string>());
  const serial = useRef(0);
  const view = state.key === key ? state.view : { items: [] };
  const pending =
    pendingState.key === key
      ? pendingState.request
      : saved<Json | null>(key, null);

  const reload = useCallback(async () => {
    if (!p.enabled || !p.id) return;
    const attempt = ++serial.current;
    const result = await api<QueueView>(
      `/api/queue?agent=${encodeURIComponent(p.id)}`,
      undefined,
      { workspaceId: p.workspaceId },
    );
    if (currentKey.current === key && attempt === serial.current) {
      setState((previous) =>
        previous.key === key &&
        result.revision &&
        previous.view.revision === result.revision
          ? previous
          : { key, view: result, loaded: true },
      );
      setFailure({ key, text: "" });
    }
  }, [key, p.enabled, p.id, p.workspaceId]);

  useEffect(() => {
    serial.current++;
    setPending({ key, request: saved<Json | null>(key, null) });
    if (!p.enabled || !p.id) return;
    let active = true;
    let polling = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      if (!active || polling) return;
      clearTimeout(timer);
      if (document.hidden || navigator.onLine === false) return;
      polling = true;
      try {
        if (!locks.current.has(key)) await reload();
      } catch (error) {
        if (active) setFailure({ key, text: errorText(error) });
      } finally {
        polling = false;
        if (active) timer = setTimeout(poll, 3000);
      }
    };
    void poll();
    const stop = onResume(() => void poll());
    return () => {
      active = false;
      clearTimeout(timer);
      stop();
    };
  }, [key, reload, p.enabled, p.id]);

  const clearRequest = async (request: Json) => {
    const next = await updateLocalDraft<Json | null>(key, null, (current) =>
      current?.request_id === request.request_id ? null : current,
    );
    if (currentKey.current === key) setPending({ key, request: next });
  };
  useEffect(() => {
    const syncPending = () =>
      setPending({ key, request: saved<Json | null>(key, null) });
    const changed = (event: StorageEvent) => {
      if (event.key === key || event.key === null) syncPending();
    };
    window.addEventListener("storage", changed);
    const stop = onResume(syncPending);
    syncPending();
    return () => {
      window.removeEventListener("storage", changed);
      stop();
    };
  }, [key]);
  const execute = async (request: Json) => {
    if (locks.current.has(key))
      throw new Error("Wait for the current queue change.");
    if (!p.id || request.agent !== p.id)
      throw new Error("The queue belongs to another chat.");
    locks.current.add(key);
    setBusy(key);
    serial.current++;
    try {
      await api("/api/queue", request, { workspaceId: p.workspaceId });
      await clearRequest(request);
      if (request.action === "cancel") p.observed?.([request.id]);
      if (request.action === "edit") p.edited?.(request.id, request.text);
      if (currentKey.current === key) {
        await reload().catch((error) =>
          setFailure({ key, text: errorText(error) }),
        );
        void p.refresh().catch(() => {});
      }
    } catch (error) {
      if (
        error instanceof ApiError &&
        [400, 401, 403, 404, 409, 422].includes(error.status)
      ) {
        await clearRequest(request);
      }
      if (currentKey.current === key) await reload().catch(() => {});
      throw error;
    } finally {
      locks.current.delete(key);
      setBusy((value) => (value === key ? null : value));
    }
  };
  const mutate = async (change: Json) => {
    const existing = saved<Json | null>(key, null);
    setPending({ key, request: existing });
    if (existing)
      throw new Error(
        "Retry the previous queue change before making another change.",
      );
    if (!view.capabilities?.receipts || !view.revision)
      throw new Error(
        "The queue is still loading or the server needs an update.",
      );
    const request = {
      ...change,
      agent: p.id,
      expected_revision: view.revision,
      request_id: crypto.randomUUID(),
    };
    // Save before HTTP so a lost response or reload reuses the same operation.
    await updateLocalDraft<Json | null>(key, null, (current) => {
      if (current)
        throw new Error(
          "Retry the previous queue change before making another change.",
        );
      return request;
    });
    setPending({ key, request });
    return execute(request);
  };
  return {
    items: view.items,
    loading: state.key !== key || !state.loaded,
    error: failure.key === key ? failure.text : "",
    canReorder: !!view.capabilities?.reorder,
    busy: busyKey === key,
    pending,
    reload,
    retry: async () => {
      const request = saved<Json | null>(key, null);
      setPending({ key, request });
      if (request) await execute(request);
    },
    edit: async (item: QueueItem, text: string) => {
      const current = view.items.find((row) => row.id === item.id);
      await mutate({
        action: "edit",
        id: item.id,
        text,
        expectedText: current?.text === text.trim() ? current.text : item.text,
      });
    },
    cancel: (item: QueueItem) =>
      mutate({ action: "cancel", id: item.id, expectedText: item.text }),
    reorder: (ids: string[]) => mutate({ action: "reorder", ordered_ids: ids }),
  };
}
