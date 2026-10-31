import { outboxRequestKeys, outboxRequests } from "./browserPersistence";
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
  const outbox = outboxRequests(localStorage);
  const suffixFor = (key: string) => {
    const prefix = outbox.key("chatCreation", "");
    if (!key.startsWith(prefix)) throw new Error("Invalid chat request scope.");
    return key.slice(prefix.length);
  };
  function pendingChatCreations(key: string): Json[] {
    const suffix = suffixFor(key);
    const requests = new Map<string, Json>();
    const read = (name: string) => {
      const request = JSON.parse(outbox.get("chatCreation", name) || "null");
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
    read(suffix);
    for (const name of outboxRequestKeys(
      localStorage,
      "chatCreation",
      requestPrefix(suffix),
    ))
      read(name);
    return [...requests.values()];
  }

  function saveChatCreation(key: string, request: Json): void {
    // Separate records preserve other projects and requests from other tabs.
    outbox.set(
      "chatCreation",
      requestPrefix(suffixFor(key)) + request.id,
      JSON.stringify(request),
    );
  }

  function confirmChatCreation(key: string, id: string): void {
    const suffix = suffixFor(key);
    outbox.remove("chatCreation", requestPrefix(suffix) + id);
    const legacy = JSON.parse(outbox.get("chatCreation", suffix) || "null");
    if (legacy?.id === id) outbox.remove("chatCreation", suffix);
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

  /** Settle a failed attempt. A definitive refusal ends the pending request; a lost
   * response keeps it for an exact-ID retry. Returns whether it was settled. */
  function settleFailedChatCreation(
    key: string,
    id: string,
    error: unknown,
    _reason: string,
  ): boolean {
    if (!isDefinitiveCreationRefusal(error)) return false;
    confirmChatCreation(key, id);
    return true;
  }

  /** The user stops tracking a request whose outcome is unknown. A chat it created
   * still appears in the chat list; Studio no longer retries this request ID. */
  function dismissChatCreation(key: string, id: string): void {
    confirmChatCreation(key, id);
  }

  return {
    pendingChatCreations,
    saveChatCreation,
    confirmChatCreation,
    confirmChatCreationResponse,
    settleFailedChatCreation,
    dismissChatCreation,
  };
}

export const {
  pendingChatCreations,
  saveChatCreation,
  confirmChatCreation,
  confirmChatCreationResponse,
  settleFailedChatCreation,
  dismissChatCreation,
} = createChatCreationStore(serverLocalStorage);
