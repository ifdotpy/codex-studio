import { ApiError } from "./api";
import { serverLocalStorage } from "./servers/storage";
import type { Json } from "./types";

const requestPrefix = (key: string) => `${key}:request:`;

/** HTTP 408 Request Timeout: the server did not finish reading the request, so it is
 * the one client-error status that leaves the creation outcome unknown. */
const REQUEST_TIMEOUT_STATUS = 408;

/** Whether a failed creation attempt has a definitive server answer proving that this
 * request ID created nothing: an explicit not-applied outcome or a client-error refusal.
 * Network failures, timeouts and server errors leave the outcome unknown. */
export function isDefinitiveCreationRefusal(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    (error.outcome === "not_applied" ||
      (error.status >= 400 &&
        error.status < 500 &&
        error.status !== REQUEST_TIMEOUT_STATUS))
  );
}

export function createChatCreationStore(localStorage: Storage) {
  function pendingChatCreations(key: string): Json[] {
    const requests = new Map<string, Json>();
    const read = (name: string) => {
      const request = JSON.parse(localStorage.getItem(name) || "null");
      if (request === null) return;
      if (
        typeof request !== "object" ||
        typeof request.id !== "string" ||
        !request.id
      )
        throw new Error("The saved chat request is invalid.");
      const existing = requests.get(request.id);
      if (existing && JSON.stringify(existing) !== JSON.stringify(request))
        throw new Error("The saved chat requests have conflicting identities.");
      requests.set(request.id, request);
    };
    // Retain the old single request until its exact receipt is confirmed.
    read(key);
    const prefix = requestPrefix(key);
    for (let index = 0; index < localStorage.length; index++) {
      const name = localStorage.key(index);
      if (name?.startsWith(prefix)) read(name);
    }
    return [...requests.values()];
  }

  function saveChatCreation(key: string, request: Json): void {
    // Separate records preserve other projects and requests from other tabs.
    localStorage.setItem(
      requestPrefix(key) + request.id,
      JSON.stringify(request),
    );
  }

  function confirmChatCreation(key: string, id: string): void {
    localStorage.removeItem(requestPrefix(key) + id);
    const legacy = JSON.parse(localStorage.getItem(key) || "null");
    if (legacy?.id === id) localStorage.removeItem(key);
  }

  function confirmChatCreationResponse(
    key: string,
    requestId: string,
    responseId: string,
  ): boolean {
    if (requestId !== responseId) return false;
    confirmChatCreation(key, requestId);
    return true;
  }

  function archiveChatCreation(
    key: string,
    id: string,
    archive: "rejected" | "dismissed",
    outcome: "not_applied" | "unknown",
    reason: string,
  ): void {
    const request = pendingChatCreations(key).find((item) => item.id === id);
    if (!request) return;
    // Preserve the exact body before the pending record stops being tracked.
    localStorage.setItem(
      `${key}:${archive}:${id}`,
      JSON.stringify({ request, outcome, reason }),
    );
    confirmChatCreation(key, id);
  }

  function rejectChatCreation(key: string, id: string, reason: string): void {
    archiveChatCreation(key, id, "rejected", "not_applied", reason);
  }

  /** Settle a failed attempt. A definitive refusal ends the pending request; a lost
   * response keeps it for an exact-ID retry. Returns whether it was settled. */
  function settleFailedChatCreation(
    key: string,
    id: string,
    error: unknown,
    reason: string,
  ): boolean {
    if (!isDefinitiveCreationRefusal(error)) return false;
    rejectChatCreation(key, id, reason);
    return true;
  }

  /** The user stops tracking a request whose outcome is unknown. A chat it created
   * still appears in the chat list; Studio no longer retries this request ID. */
  function dismissChatCreation(key: string, id: string): void {
    archiveChatCreation(
      key,
      id,
      "dismissed",
      "unknown",
      "Dismissed by the user",
    );
  }

  return {
    pendingChatCreations,
    saveChatCreation,
    confirmChatCreation,
    confirmChatCreationResponse,
    rejectChatCreation,
    settleFailedChatCreation,
    dismissChatCreation,
  };
}

export const {
  pendingChatCreations,
  saveChatCreation,
  confirmChatCreation,
  confirmChatCreationResponse,
  rejectChatCreation,
  settleFailedChatCreation,
  dismissChatCreation,
} = createChatCreationStore(serverLocalStorage);
