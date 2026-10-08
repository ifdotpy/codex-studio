import { serverLocalStorage as localStorage } from "./servers/storage";
import type { Json } from "./types";

const requestPrefix = (key: string) => `${key}:request:`;

export function pendingChatCreations(key: string): Json[] {
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

export function saveChatCreation(key: string, request: Json): void {
  // Separate records preserve other projects and requests from other tabs.
  localStorage.setItem(
    requestPrefix(key) + request.id,
    JSON.stringify(request),
  );
}

export function confirmChatCreation(key: string, id: string): void {
  localStorage.removeItem(requestPrefix(key) + id);
  const legacy = JSON.parse(localStorage.getItem(key) || "null");
  if (legacy?.id === id) localStorage.removeItem(key);
}
