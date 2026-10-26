import { useSyncExternalStore } from "react";
import { ApiError } from "../apiErrors";
import type { PostBody } from "../api";
import type { Attachment } from "../components/ComposerAttachments";
import type { MessageReceipt } from "../sync/messageReceipts";
import type { OutboxEntry } from "../sync/send";
import type { NativeServerContext } from "./client";

type Context = Pick<
  NativeServerContext,
  "serverId" | "workspaceId" | "request" | "subscribe"
>;
type Entry = OutboxEntry & { workspace: string };
type View = { entries: OutboxEntry[]; error: string };
type MessageBody = PostBody<"/api/messages">;
const statuses = {
  queued: true,
  paused: true,
  cancelled: true,
  accepted: true,
  uncertain: true,
  failed: true,
} satisfies Record<OutboxEntry["status"], true>;

function object(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
function message(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
// Rust JSON objects can return their keys in another order. Arrays retain order.
function sameJSON(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (Array.isArray(left) && Array.isArray(right))
    return (
      left.length === right.length &&
      left.every((value, index) => sameJSON(value, right[index]))
    );
  if (!object(left) || !object(right)) return false;
  const keys = Object.keys(left);
  return (
    keys.length === Object.keys(right).length &&
    keys.every(
      (key) => Object.hasOwn(right, key) && sameJSON(left[key], right[key]),
    )
  );
}

/** Rust owns queue storage, claims, delivery, retry, and removal fences. */
export function createNativeOutbox(context: Context) {
  const {
    serverId,
    workspaceId: workspace,
    request,
    subscribe: subscribeNative,
  } = context;
  if (!serverId || !/^[a-f0-9]{32}$/.test(workspace))
    throw new Error("The native queue owner is invalid.");
  const listeners = new Set<() => void>();
  let snapshot: View = { entries: [], error: "" };
  let generation = 0;
  let closeView: (() => Promise<void>) | undefined;
  let activation: Promise<void> | undefined;
  const openings = new Set<Promise<void>>();
  const closing = new Set<Promise<void>>();
  const cleanupErrors: unknown[] = [];
  let writing: Promise<void> = Promise.resolve();
  let disposed = false;

  function publish(next: View) {
    snapshot = next;
    for (const listener of listeners) listener();
  }
  function report(error: unknown) {
    publish({ ...snapshot, error: message(error) });
  }
  function parseEntry(value: unknown): Entry {
    if (
      !object(value) ||
      value.workspace !== workspace ||
      typeof value.id !== "string" ||
      !value.id ||
      !object(value.body) ||
      value.body.id !== value.id ||
      typeof value.body.room !== "string" ||
      !value.body.room ||
      typeof value.status !== "string" ||
      !Object.hasOwn(statuses, value.status) ||
      !Number.isSafeInteger(value.created) ||
      (value.created as number) < 0 ||
      typeof value.displayPending !== "boolean" ||
      !Array.isArray(value.attachments) ||
      (value.attempted !== null &&
        value.attempted !== undefined &&
        typeof value.attempted !== "boolean") ||
      (value.receipt !== null &&
        value.receipt !== undefined &&
        (!object(value.receipt) || value.receipt.id !== value.id))
    )
      throw new Error("The native queue entry does not match its owner.");
    return value as Entry;
  }
  async function readEntries(): Promise<Entry[]> {
    const value = await request<unknown>("outbox.list", { workspace });
    if (!Array.isArray(value))
      throw new Error("The native queue response is invalid.");
    return value.map(parseEntry);
  }
  function intention(entry: Entry) {
    if (entry.status === "failed")
      throw new ApiError(entry.error || "Message failed.", 400);
    return entry.receipt
      ? {
          ...entry.receipt,
          kind: "server" as const,
          queued: entry.receipt.status === "queued",
        }
      : {
          kind: "local" as const,
          id: entry.id,
          status: entry.status,
          queued: entry.status === "queued" || entry.status === "paused",
          error: entry.error,
        };
  }
  async function savedWrite<T>(
    method: string,
    params: Record<string, unknown>,
    identity: string,
    retain = false,
    validate?: (value: T) => void,
  ): Promise<T> {
    // Explicit local actions keep their order. Delivery remains a Rust concern.
    const previous = writing;
    const operation = previous.then(async () => {
      const key = JSON.stringify([
        "native-outbox",
        serverId,
        workspace,
        retain
          ? method
          : method === "outbox.acknowledge_many"
            ? "acknowledge"
            : "message",
        identity,
      ]);
      const id = await request<string>(
        retain ? "mutation.stable_id" : "mutation.write_id",
        retain
          ? { key }
          : { key, serializedPayload: JSON.stringify({ method, params }) },
      );
      if (typeof id !== "string" || !id || id.length > 128)
        throw new Error("The native queue request identity is invalid.");
      const result = await request<T>(method, params, id);
      validate?.(result);
      if (!retain)
        await request("mutation.clear_write_id", { key, requestId: id });
      return result;
    });
    writing = operation.then(
      () => {},
      () => {},
    );
    return operation;
  }
  async function durableSend(
    body: MessageBody,
    attachments: Attachment[] = [],
    onPersist?: () => void | Promise<void>,
  ) {
    const frozen = JSON.parse(JSON.stringify(body)) as MessageBody;
    const savedAttachments = JSON.parse(
      JSON.stringify(attachments),
    ) as Attachment[];
    if (typeof frozen.id !== "string" || !frozen.id)
      throw new Error("A message identity is required.");
    let entry: Entry;
    let uncertain = false;
    try {
      entry = parseEntry(
        await savedWrite(
          "outbox.enqueue",
          { workspace, body: frozen, attachments: savedAttachments },
          frozen.id,
          true,
        ),
      );
    } catch (error) {
      const found = (await readEntries()).find(
        (entry) => entry.id === frozen.id,
      );
      if (!found) throw error;
      entry = found;
      uncertain = true;
    }
    if (!sameJSON(entry.body, frozen))
      throw new Error("Message identity already has different content.");
    if (entry.status === "failed") return intention(entry);
    await onPersist?.();
    if (uncertain || entry.status !== "queued") return intention(entry);
    let failure: unknown;
    try {
      await request("outbox.deliver", { workspace, id: frozen.id });
    } catch (error) {
      failure = error;
    }
    const settled = (await readEntries()).find(
      (entry) => entry.id === frozen.id,
    );
    if (!settled)
      throw new Error("The native queue lost the saved message identity.");
    if (!sameJSON(settled.body, frozen))
      throw new Error("The native queue returned different message content.");
    const result = intention(settled);
    return failure && result.kind === "local" && !result.error
      ? { ...result, error: message(failure) }
      : result;
  }
  async function acknowledgeOutbox(ids: string[]) {
    await savedWrite(
      "outbox.acknowledge_many",
      { workspace, ids: [...ids] },
      "acknowledge",
      false,
      (value) => {
        if (!object(value) || value.acknowledged !== true)
          throw new Error("The native acknowledgement is invalid.");
      },
    );
  }
  async function editOutboxDisplay(id: string, text: string) {
    await savedWrite(
      "outbox.display",
      { workspace, id, text },
      id,
      false,
      parseEntry,
    );
  }
  async function changeOutbox(
    id: string,
    action: "cancel" | "pause" | "resume",
  ) {
    await savedWrite(
      "outbox.change",
      { workspace, id, action },
      id,
      false,
      parseEntry,
    );
  }
  async function stopRemovedMessage(id: string): Promise<boolean> {
    const result = await savedWrite<unknown>(
      "outbox.stop_removed",
      { workspace, id },
      id,
      false,
      (value) => {
        if (typeof value !== "boolean")
          throw new Error("The native removal receipt is invalid.");
      },
    );
    return result === true;
  }
  async function reconcileOutboxReceipts(
    room: string,
    receipts: MessageReceipt[],
    workspaceId: string,
    signal?: AbortSignal,
  ) {
    if (workspaceId !== workspace || signal?.aborted) return;
    const entries = await readEntries();
    for (const receipt of receipts) {
      if (signal?.aborted) return;
      const entry = entries.find((entry) => entry.id === receipt.id);
      if (
        !entry ||
        entry.body.room !== room ||
        receipt.agent !== room ||
        !(
          ["accepted", "uncertain"].includes(entry.status) ||
          (entry.attempted === true &&
            ["queued", "paused"].includes(entry.status))
        )
      )
        continue;
      const updated = parseEntry(
        await request("outbox.receipt", { workspace, id: entry.id, receipt }),
      );
      if (signal?.aborted) return;
      if (
        updated.status === "accepted" &&
        updated.receipt?.status === "delivered" &&
        updated.receipt.materialized === true
      )
        await acknowledgeOutbox([entry.id]);
    }
  }
  function close(close: () => Promise<void>) {
    const operation = close().catch((error) => {
      cleanupErrors.push(error);
      report(error);
    });
    closing.add(operation);
    void operation.finally(() => closing.delete(operation));
  }
  function open() {
    const current = ++generation;
    const opening = subscribeNative<unknown>("outbox", {}, (event) => {
      if (current !== generation || disposed) return;
      if (event.error) {
        report(event.error.message);
        return;
      }
      if (!object(event.value) || event.value.workspaceId !== workspace) {
        report("The native queue view belongs to another workspace.");
        return;
      }
      try {
        if (!Array.isArray(event.value.entries))
          throw new Error("The native queue view is invalid.");
        publish({ entries: event.value.entries.map(parseEntry), error: "" });
      } catch (error) {
        report(error);
      }
    })
      .then((stop) => {
        if (current !== generation || !listeners.size || disposed) close(stop);
        else closeView = stop;
      })
      .catch(report);
    openings.add(opening);
    void opening.finally(() => openings.delete(opening));
    if (!activation) {
      activation = request("outbox.activate", {}).then(
        () => {},
        (error) => {
          activation = undefined;
          report(error);
        },
      );
    }
  }
  function watch(listener: () => void) {
    if (disposed) throw new Error("The native queue view is closed.");
    listeners.add(listener);
    if (listeners.size === 1) open();
    return () => {
      listeners.delete(listener);
      if (!listeners.size) {
        generation++;
        if (closeView) {
          const stop = closeView;
          closeView = undefined;
          close(stop);
        }
      }
    };
  }
  return {
    durableSend,
    acknowledgeOutbox,
    editOutboxDisplay,
    reconcileOutboxReceipts,
    changeOutbox,
    stopRemovedMessage,
    useOutbox: () =>
      useSyncExternalStore(
        watch,
        () => snapshot,
        () => snapshot,
      ),
    async dispose() {
      disposed = true;
      generation++;
      listeners.clear();
      if (closeView) {
        const stop = closeView;
        closeView = undefined;
        close(stop);
      }
      while (openings.size) await Promise.all(openings);
      while (closing.size) await Promise.all(closing);
      if (cleanupErrors.length)
        throw new AggregateError(
          cleanupErrors,
          "The native queue view did not close.",
        );
    },
  };
}
