import { randomUUID } from "../uuid";
import { sha256Hex } from "../sha256";
import { serverViewId } from "./environment";
import { serverLocalStorage } from "./storage";
import {
  createMonitorInputStore,
  type MonitorInputEntry,
  type MonitorInputStore,
} from "./monitorInputStore";
const MONITOR_INPUT_PATH = "/api/monitor/input";
const UNCERTAIN_RESPONSE_CODES = new Set([
  "remote_timeout",
  "remote_unavailable",
  "remote_response",
  "response_size",
  "access_unavailable",
  "outcome_unknown",
  "request_pending",
  "receipt_expired",
  "request_conflict",
]);

type MonitorJournalOwner = { readonly monitorInput: MonitorInputStore };

export async function pendingMonitorInput(
  journal: MonitorJournalOwner,
  monitorId: string,
): Promise<{ requestId: string } | null> {
  return journal.monitorInput.transition(monitorId, (entry) => ({
    entry,
    result: entry ? { requestId: entry.requestId } : null,
  }));
}

// The caller must first verify an exact acknowledged or rejected process receipt.
export async function finishMonitorInput(
  journal: MonitorJournalOwner,
  monitorId: string,
  requestId: string,
): Promise<boolean> {
  return journal.monitorInput.transition(monitorId, (entry) => {
    const matches = entry?.requestId === requestId;
    return { entry: matches ? undefined : entry, result: matches };
  });
}

export function serverMutationResponseIsUncertain(
  status: number,
  body: unknown,
) {
  if (status >= 500 || status === 408 || status === 429) return true;
  const code =
    body && typeof body === "object"
      ? String((body as Record<string, unknown>).code || "")
      : "";
  return UNCERTAIN_RESPONSE_CODES.has(code);
}

// Only unresolved writes retain this identity. Explicit operation identities
// remain the caller's source of truth, including sends, uploads and creation.
export function createMutationJournal(
  serverId: string | null,
  storage: Storage,
  monitorInput: MonitorInputStore = createMonitorInputStore(serverId),
) {
  async function beginMonitorInput(body: unknown, explicitRequestId?: string) {
    const value =
      body && typeof body === "object" ? (body as Record<string, unknown>) : {};
    if (typeof value.id !== "string" || !value.id) {
      throw new Error("A monitor ID is required for input.");
    }
    const monitorId = value.id;
    const serialized = JSON.stringify(body);
    const payloadDigest = await sha256Hex(
      new TextEncoder().encode(MONITOR_INPUT_PATH + "\n" + serialized),
    );
    if (JSON.stringify(body) !== serialized) {
      throw new Error("Monitor input changed before its identity was saved.");
    }
    const explicit = [
      explicitRequestId,
      value.requestId,
      value.request_id,
    ].find((id): id is string => typeof id === "string" && !!id);
    const entry = await monitorInput.transition(monitorId, (saved) => {
      if (JSON.stringify(body) !== serialized) {
        throw new Error("Monitor input changed before its identity was saved.");
      }
      if (
        saved &&
        (saved.payloadDigest !== payloadDigest ||
          (explicit && saved.requestId !== explicit))
      ) {
        throw new Error(
          "Resolve the pending monitor input before sending different input.",
        );
      }
      // A saved unresolved attempt can have reached the process before a reload.
      const next: MonitorInputEntry = saved
        ? { ...saved, uncertain: true }
        : {
            requestId: explicit || randomUUID(),
            payloadDigest,
            uncertain: false,
          };
      return { entry: next, result: next };
    });
    const updateCurrent = (
      update: (current: MonitorInputEntry) => MonitorInputEntry | undefined,
    ) =>
      monitorInput.transition(monitorId, (current) => ({
        entry:
          current?.requestId === entry.requestId &&
          current.payloadDigest === entry.payloadDigest
            ? update(current)
            : current,
        result: undefined,
      }));
    const finish = () => updateCurrent(() => undefined);
    const unknown = () =>
      updateCurrent((current) => ({ ...current, uncertain: true }));
    return {
      requestId: entry.requestId,
      finish,
      unknown,
      reject(status: number, body: unknown) {
        return updateCurrent((current) =>
          serverMutationResponseIsUncertain(status, body)
            ? { ...current, uncertain: true }
            : current.uncertain
              ? current
              : undefined,
        );
      },
    };
  }

  async function beginServerMutation(
    path: string,
    body: unknown,
    explicitRequestId?: string,
  ) {
    // Session control recovers by reopening the full stream; dead sessions have no durable receipt.
    if (path === "/api/sync/subscriptions") return null;
    if (path === MONITOR_INPUT_PATH)
      return beginMonitorInput(body, explicitRequestId);
    if (explicitRequestId)
      return {
        requestId: explicitRequestId,
        finish() {},
        unknown() {},
        reject(_status: number, _body: unknown) {},
      };
    if (!serverId || serverId === "local") return null;
    const value =
      body && typeof body === "object" ? (body as Record<string, unknown>) : {};
    const explicit = [
      value.requestId,
      value.request_id,
      ...(["/api/messages", "/api/leads", "/api/assets"].includes(path)
        ? [value.id]
        : []),
    ].find((id): id is string => typeof id === "string" && !!id);
    if (explicit)
      return {
        requestId: explicit,
        finish() {},
        unknown() {},
        reject(_status: number, _body: unknown) {},
      };
    const hash = await sha256Hex(
      new TextEncoder().encode(path + "\n" + JSON.stringify(body)),
    );
    const key = `studio-server-write:${hash}`;
    const existing = storage.getItem(key);
    let saved: { requestId: string; uncertain: boolean } | undefined;
    if (existing) {
      try {
        saved = JSON.parse(existing);
      } catch {
        saved = { requestId: existing, uncertain: true };
      }
    }
    const entry = saved || { requestId: randomUUID(), uncertain: false };
    const requestId = entry.requestId;
    const save = () => storage.setItem(key, JSON.stringify(entry));
    save();
    const finish = () => {
      const current = storage.getItem(key);
      if (current === JSON.stringify(entry)) storage.removeItem(key);
    };
    return {
      requestId,
      finish,
      unknown() {
        entry.uncertain = true;
        save();
      },
      reject(status: number, body: unknown) {
        if (serverMutationResponseIsUncertain(status, body)) {
          entry.uncertain = true;
          save();
        } else if (!entry.uncertain) finish();
      },
    };
  }

  return Object.defineProperty(beginServerMutation, "monitorInput", {
    value: monitorInput,
  }) as typeof beginServerMutation & MonitorJournalOwner;
}
export const beginServerMutation = createMutationJournal(
  serverViewId,
  serverLocalStorage,
);
