import { useEffect, useState } from "react";
import { errorText, type PostBody } from "../api";
import { randomUUID } from "../uuid";
import { onResume } from "../sync/resume";
import {
  intentionResult,
  type OutboxEntry,
  type OutboxService,
} from "../sync/send";
import { listenNativeEvents, nativeRequest } from "./client";

type LegacyOutbox = () => Promise<{
  workspaceId: string;
  db: {
    outbox: { find(): { exec(): Promise<{ id: string; payload: string }[]> } };
  };
}>;

type MessageBody = PostBody<"/api/messages">;

let identity: Promise<string> | undefined;
/** The local workspace of the core, read once until a read fails. */
function workspaceId(): Promise<string> {
  return (identity ??= (async () => {
    const cached = await nativeRequest<{ workspaceId: string } | null>(
      "identity.get",
      {},
    );
    const current =
      cached ?? (await nativeRequest<{ workspaceId: string }>("sync.open", {}));
    return current.workspaceId;
  })().catch((error: unknown) => {
    identity = undefined;
    throw error;
  }));
}

/**
 * The pending send of a room, stored by the core before any HTTP request. The
 * same text returns the same identity until `releaseMessage` removes it, so a
 * resend after a lost reply reaches `outbox.enqueue` with the stored identity.
 */
export async function prepareMessage(
  message: Omit<MessageBody, "id">,
): Promise<MessageBody> {
  const body = await nativeRequest<MessageBody>("outbox.prepare", {
    workspace: await workspaceId(),
    ...message,
  });
  if (!body.id)
    throw new Error("The native client returned no message identity.");
  return body;
}

/** Removes the pending send after the outbox owns it or it was refused. */
export async function releaseMessage(room: string, id: string): Promise<void> {
  await nativeRequest("outbox.release", {
    workspace: await workspaceId(),
    room,
    id,
  });
}

/**
 * The message queue of the local server in the native client. The core owns
 * queue states, delivery, retry, receipts, and remove and restore
 * (`client-core/src/outbox.rs`, `delivery.rs`). The page only renders entries
 * and forwards user commands. The server deduplicates each message ID, so a
 * retry with the same identity never sends a user message twice.
 */
export function createNativeOutbox(legacyOutbox: LegacyOutbox): OutboxService {
  const workspace = workspaceId;

  const entry = async (workspaceId: string, id: string) =>
    (
      await nativeRequest<OutboxEntry[]>("outbox.list", {
        workspace: workspaceId,
      })
    ).find((item) => item.id === id);

  const durableSend: OutboxService["durableSend"] = async (
    body,
    attachments = [],
    onPersist,
  ) => {
    if (typeof body.id !== "string" || !body.id)
      throw new Error("A message identity is required");
    const workspaceId = await workspace();
    // A lost reply replays the stored result of this exact request.
    await nativeRequest(
      "outbox.enqueue",
      { workspace: workspaceId, body, attachments },
      `outbox.enqueue:${body.id}`,
    );
    await onPersist?.();
    let failure: unknown;
    await nativeRequest("outbox.deliver", {
      workspace: workspaceId,
      id: body.id,
    }).catch((error: unknown) => (failure = error));
    // The stored entry is the outcome; the core retries a queued message.
    const stored = await entry(workspaceId, body.id);
    if (!stored) throw failure ?? new Error("The message left this device.");
    return intentionResult(
      failure && stored.status === "queued" && !stored.error
        ? { ...stored, error: errorText(failure) }
        : stored,
    );
  };

  const command = async (method: string, params: Record<string, unknown>) =>
    nativeRequest(method, { workspace: await workspace(), ...params });

  const changeOutbox: OutboxService["changeOutbox"] = async (id, action) => {
    await command("outbox.change", { id, action });
    if (action === "resume")
      void command("outbox.deliver", { id }).catch(() => {});
  };

  async function importLegacy(workspaceId: string) {
    const legacy = await legacyOutbox();
    if (legacy.workspaceId !== workspaceId) return [];
    const unreadable: string[] = [];
    for (const row of await legacy.db.outbox.find().exec()) {
      try {
        // The core decodes and validates the row.
        const value: unknown = JSON.parse(row.payload);
        if (!value || typeof value !== "object" || Array.isArray(value))
          throw new Error("Invalid saved message.");
        await nativeRequest("outbox.import", {
          workspace: workspaceId,
          entries: [{ ...value, id: row.id }],
        });
      } catch {
        unreadable.push(row.id);
      }
    }
    return unreadable;
  }

  function useOutbox() {
    const [entries, setEntries] = useState<OutboxEntry[]>([]);
    const [error, setError] = useState("");
    const [legacyError, setLegacyError] = useState("");
    useEffect(() => {
      let stopped = false;
      const subscription = randomUUID();
      let unlisten = () => {};
      const recover = () =>
        void nativeRequest("outbox.recover", {}).catch(() => {});
      void (async () => {
        const workspaceId = await workspace();
        // Old browser rows keep their state; the core accepts each once.
        const unreadable = await importLegacy(workspaceId).catch(() => []);
        if (stopped) return;
        setLegacyError(
          unreadable.length
            ? `Saved messages could not be read; delivery is uncertain: ${unreadable.join(", ")}.`
            : "",
        );
        const stop = await listenNativeEvents((input) => {
          const message = input as {
            event?: string;
            value?: { entries?: OutboxEntry[] };
            error?: { message?: string };
          } | null;
          if (stopped || message?.event !== subscription) return;
          if (message.error)
            return setError(
              message.error.message || "The queue is unavailable.",
            );
          setError("");
          setEntries(message.value?.entries ?? []);
        });
        if (stopped) return stop();
        unlisten = stop;
        await nativeRequest("view.subscribe", {
          subscription,
          topic: "outbox",
          workspace: workspaceId,
        });
        // A cleanup can race the subscription request.
        if (stopped)
          return void nativeRequest("view.unsubscribe", { subscription }).catch(
            () => {},
          );
        await nativeRequest("outbox.activate", {});
        recover();
      })().catch((failure: unknown) => {
        if (!stopped) setError(errorText(failure));
      });
      const stopResume = onResume(recover);
      return () => {
        stopped = true;
        stopResume();
        unlisten();
        void nativeRequest("view.unsubscribe", { subscription }).catch(
          () => {},
        );
      };
    }, []);
    return { entries, error: legacyError || error };
  }

  return {
    durableSend,
    acknowledgeOutbox: (ids) =>
      command("outbox.acknowledge_many", { ids }).then(() => {}),
    reconcileOutboxReceipts: async (room, receipts, workspaceId, signal) => {
      if (signal?.aborted || workspaceId !== (await workspace())) return;
      await command("outbox.reconcile", { room, receipts });
    },
    editOutboxDisplay: (id, text) =>
      command("outbox.display", { id, text }).then(() => {}),
    changeOutbox,
    stopRemovedMessage: async (id) =>
      (await command("outbox.stop", { id }).then(
        (value) => (value as { cancelled: boolean }).cancelled,
      )) === true,
    useOutbox,
  };
}
