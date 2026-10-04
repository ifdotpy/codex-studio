import { useCallback, useEffect, useRef, useState } from "react";
import { get, syncPost, ApiError, errorText } from "../api";
import type { QueueItemDto } from "../api";
import { updateLocalDraft } from "../sync/localDraft";
import { onResume } from "../sync/resume";
import type { paths } from "../generated/api";
import { queueMutationMessageId } from "./queueMutationIdentity";

type QueueView =
  paths["/api/queue"]["get"]["responses"][200]["content"]["application/json"];
type QueueMutation =
  paths["/api/queue"]["post"]["requestBody"]["content"]["application/json"];
type QueueChange<T = QueueMutation> = T extends unknown
  ? Omit<T, "agent" | "expected_revision" | "request_id">
  : never;

const emptyQueue: QueueView = {
  items: [],
  revision: "",
  capabilities: { reorder: false, receipts: false },
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isStringArray(value: unknown): value is string[] {
  return (
    Array.isArray(value) && value.every((item) => typeof item === "string")
  );
}

function readQueueRequest(key: string): unknown {
  let stored: string | null;
  try {
    stored = localStorage.getItem(key);
  } catch {
    return "";
  }
  if (stored === null) return null;
  try {
    const request: unknown = JSON.parse(stored);
    return request;
  } catch {
    return stored;
  }
}

function isQueueMutation(value: unknown): value is QueueMutation {
  if (
    !isRecord(value) ||
    typeof value.agent !== "string" ||
    typeof value.request_id !== "string" ||
    typeof value.expected_revision !== "string"
  )
    return false;
  const hasMessageId =
    typeof value.message_id === "string" || typeof value.id === "string";
  if (value.action === "cancel" || value.action === "send_now")
    return hasMessageId && typeof value.expectedText === "string";
  if (value.action === "edit")
    return (
      hasMessageId &&
      typeof value.expectedText === "string" &&
      typeof value.text === "string"
    );
  if (value.action === "first") return hasMessageId;
  return value.action === "reorder" && isStringArray(value.ordered_ids);
}

export function useMessageQueue(p: {
  id: string | null;
  enabled: boolean;
  scope: string;
  workspaceId?: string;
  observed?: (ids: string[]) => void;
  edited?: (id: string, text: string) => void;
  refresh: () => Promise<void>;
  pendingDelivery?: boolean;
  refreshDelivery?: () => Promise<void>;
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
    view: emptyQueue,
  });
  const [failure, setFailure] = useState({ key, text: "" });
  const [pendingState, setPending] = useState<{
    key: string;
    request: unknown;
  }>({
    key,
    request: readQueueRequest(key),
  });
  const [busyKey, setBusy] = useState<string | null>(null);
  const locks = useRef(new Set<string>());
  const serial = useRef(0);
  const view = state.key === key ? state.view : emptyQueue;
  const pending =
    pendingState.key === key ? pendingState.request : readQueueRequest(key);

  const reload = useCallback(async () => {
    if (!p.enabled || !p.id) return;
    const attempt = ++serial.current;
    const result = await get("/api/queue", {
      query: { agent: p.id },
      workspaceId: p.workspaceId,
    });
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
    setPending({ key, request: readQueueRequest(key) });
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
        // A committed send can outlive a lost stream notification. Reconcile
        // only while this chat has an unresolved delivery, through shared sync.
        if (active && p.pendingDelivery) await p.refreshDelivery?.();
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
  }, [key, reload, p.enabled, p.id, p.pendingDelivery, p.refreshDelivery]);

  const clearRequest = async (request: QueueMutation) => {
    const next = await updateLocalDraft<unknown>(key, null, (current) =>
      isQueueMutation(current) && current.request_id === request.request_id
        ? null
        : current,
    );
    if (currentKey.current === key) setPending({ key, request: next });
  };
  useEffect(() => {
    const syncPending = () =>
      setPending({ key, request: readQueueRequest(key) });
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
  const execute = async (request: QueueMutation) => {
    if (locks.current.has(key))
      throw new Error("Wait for the current queue change.");
    if (!p.id || request.agent !== p.id)
      throw new Error("The queue belongs to another chat.");
    locks.current.add(key);
    setBusy(key);
    serial.current++;
    try {
      await syncPost("/api/queue", request, {
        workspaceId: p.workspaceId,
      });
      await clearRequest(request);
      const messageId = queueMutationMessageId(request);
      if (request.action === "cancel" && messageId) p.observed?.([messageId]);
      if (
        request.action === "edit" &&
        messageId &&
        typeof request.text === "string"
      )
        p.edited?.(messageId, request.text);
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
  const mutate = async (change: QueueChange) => {
    if (!p.id) throw new Error("The queue belongs to another chat.");
    const existing = readQueueRequest(key);
    setPending({ key, request: existing });
    if (existing !== null)
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
    } satisfies QueueMutation;
    // Save before HTTP so a lost response or reload reuses the same operation.
    await updateLocalDraft<unknown>(key, null, (current) => {
      if (current !== null)
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
      const request = readQueueRequest(key);
      setPending({ key, request });
      if (request !== null) {
        if (!isQueueMutation(request))
          throw new Error("The saved queue change cannot be verified.");
        await execute(request);
      }
    },
    edit: async (item: QueueItemDto, text: string) => {
      const current = view.items.find((row) => row.id === item.id);
      await mutate({
        action: "edit",
        id: item.id,
        text,
        expectedText: current?.text === text.trim() ? current.text : item.text,
      });
    },
    cancel: (item: QueueItemDto) =>
      mutate({ action: "cancel", id: item.id, expectedText: item.text }),
    sendNow: (item: QueueItemDto) =>
      mutate({ action: "send_now", id: item.id, expectedText: item.text }),
    reorder: (ids: string[]) => mutate({ action: "reorder", ordered_ids: ids }),
  };
}
