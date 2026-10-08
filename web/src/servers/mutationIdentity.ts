import { serverViewId } from "./environment";
import { serverLocalStorage } from "./storage";
// Only unresolved writes retain this identity. Explicit operation identities
// remain the caller's source of truth, including sends, uploads and creation.
export async function beginServerMutation(path: string, body: unknown) {
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
  if (explicit) return { requestId: explicit, finish() {} };
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
  const requestId = existing || crypto.randomUUID();
  serverLocalStorage.setItem(key, requestId);
  return {
    requestId,
    finish() {
      if (serverLocalStorage.getItem(key) === requestId)
        serverLocalStorage.removeItem(key);
    },
  };
}
