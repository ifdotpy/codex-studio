import { randomUUID } from "../uuid";
import { sha256Hex } from "../sha256";
import { serverViewId } from "./environment";
import { serverLocalStorage } from "./storage";
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

type MonitorInputEntry = {
  requestId: string;
  payloadDigest: string;
  uncertain: boolean;
};

function monitorInputJournalKey(monitorId: string): string {
  if (!monitorId) throw new Error("A monitor ID is required for input.");
  return `studio-monitor-input:${monitorId}`;
}

function readMonitorInput(
  storage: Storage,
  key: string,
): MonitorInputEntry | undefined {
  const raw = storage.getItem(key);
  if (raw === null) return;
  try {
    const entry = JSON.parse(raw);
    if (
      entry &&
      typeof entry === "object" &&
      Object.keys(entry).length === 3 &&
      typeof entry.requestId === "string" &&
      entry.requestId &&
      typeof entry.payloadDigest === "string" &&
      /^[0-9a-f]{64}$/.test(entry.payloadDigest) &&
      typeof entry.uncertain === "boolean"
    )
      return entry as MonitorInputEntry;
  } catch {}
  throw new Error(
    "The monitor input journal is invalid. Do not send another input until its result is confirmed.",
  );
}

export function pendingMonitorInput(
  storage: Storage,
  monitorId: string,
): { requestId: string } | null {
  const entry = readMonitorInput(storage, monitorInputJournalKey(monitorId));
  return entry ? { requestId: entry.requestId } : null;
}

// The caller must first verify an exact acknowledged or rejected process receipt.
export function finishMonitorInput(
  storage: Storage,
  monitorId: string,
  requestId: string,
): boolean {
  const key = monitorInputJournalKey(monitorId);
  const entry = readMonitorInput(storage, key);
  if (!entry || entry.requestId !== requestId) return false;
  storage.removeItem(key);
  return true;
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
) {
  async function beginMonitorInput(body: unknown, explicitRequestId?: string) {
    const value =
      body && typeof body === "object" ? (body as Record<string, unknown>) : {};
    if (typeof value.id !== "string" || !value.id) {
      throw new Error("A monitor ID is required for input.");
    }
    const key = monitorInputJournalKey(value.id);
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
    const saved = readMonitorInput(storage, key);
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
    const entry: MonitorInputEntry = saved
      ? { ...saved, uncertain: true }
      : {
          requestId: explicit || randomUUID(),
          payloadDigest,
          uncertain: false,
        };
    storage.setItem(key, JSON.stringify(entry));
    const currentEntry = () => {
      const current = readMonitorInput(storage, key);
      return current?.requestId === entry.requestId &&
        current.payloadDigest === entry.payloadDigest
        ? current
        : undefined;
    };
    const finish = () => {
      if (currentEntry()) storage.removeItem(key);
    };
    const unknown = () => {
      const current = currentEntry();
      if (current)
        storage.setItem(key, JSON.stringify({ ...current, uncertain: true }));
    };
    return {
      requestId: entry.requestId,
      finish,
      unknown,
      reject(status: number, body: unknown) {
        if (serverMutationResponseIsUncertain(status, body)) unknown();
        else if (currentEntry()?.uncertain === false) finish();
      },
    };
  }

  async function beginServerMutation(
    path: string,
    body: unknown,
    explicitRequestId?: string,
  ) {
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

  return beginServerMutation;
}
export const beginServerMutation = createMutationJournal(
  serverViewId,
  serverLocalStorage,
);
