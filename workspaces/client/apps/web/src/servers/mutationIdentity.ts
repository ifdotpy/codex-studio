import { serverViewId } from "./environment";
import { serverLocalStorage } from "./storage";
// Only unresolved writes retain this identity. Explicit operation identities
// remain the caller's source of truth, including sends, uploads and creation.
export async function beginServerMutation(
  path: string,
  body: unknown,
  explicitRequestId?: string,
) {
  if (explicitRequestId)
    return {
      requestId: explicitRequestId,
      finish() {},
      unknown() {},
      reject(_status: number, _body: unknown) {},
    };
  if (!serverViewId || serverViewId === "local") return null;
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
  const hash = Array.from(
    new Uint8Array(
      await crypto.subtle.digest(
        "SHA-256",
        new TextEncoder().encode(path + "\n" + JSON.stringify(body)),
      ),
    ),
    (byte) => byte.toString(16).padStart(2, "0"),
  ).join("");
  const key = `studio-server-write:${hash}`;
  const existing = serverLocalStorage.getItem(key);
  let saved: { requestId: string; uncertain: boolean } | undefined;
  if (existing) {
    try {
      saved = JSON.parse(existing);
    } catch {
      saved = { requestId: existing, uncertain: true };
    }
  }
  const entry = saved || { requestId: crypto.randomUUID(), uncertain: false };
  const requestId = entry.requestId;
  const save = () => serverLocalStorage.setItem(key, JSON.stringify(entry));
  save();
  const finish = () => {
    const current = serverLocalStorage.getItem(key);
    if (current === JSON.stringify(entry)) serverLocalStorage.removeItem(key);
  };
  return {
    requestId,
    finish,
    unknown() {
      entry.uncertain = true;
      save();
    },
    reject(status: number, body: unknown) {
      const code =
        body && typeof body === "object"
          ? String((body as Record<string, unknown>).code || "")
          : "";
      const uncertain = new Set([
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
      if (status >= 500 || uncertain.has(code)) {
        entry.uncertain = true;
        save();
      } else if (!entry.uncertain) finish();
    },
  };
}
