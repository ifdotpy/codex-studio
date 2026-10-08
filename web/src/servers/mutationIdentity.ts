import { serverViewId } from "./environment";
import { serverLocalStorage } from "./storage";
// This transport receipt identity is separate from operation IDs in the body.
// Only unresolved writes retain it; the body remains the source of truth for
// sends, uploads, login flows and other application operations.
export async function beginServerMutation(path: string, body: unknown) {
  if (!serverViewId || serverViewId === "local") return null;
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
