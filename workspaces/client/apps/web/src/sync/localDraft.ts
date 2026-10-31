import { createQueueOutbox } from "../browserPersistence";
import { serverStorageName } from "../servers/environment";
import { serverLocalStorage } from "../servers/storage";
export const LOCAL_DRAFT_DATABASE_NAME = "codex-studio-draft-writes";

export function createLocalDraftStorage(
  localStorage: Storage,
  databaseName: string,
) {
  // Synchronous writes close the gap between an input event and page teardown.
  // Keep the in-memory value when storage fails, and tell the caller to show it.
  function writeLocalDraft(key: string, value: unknown): string | null {
    try {
      if (value === null) localStorage.removeItem(key);
      else localStorage.setItem(key, JSON.stringify(value));
      return null;
    } catch {
      return "Changes could not be saved on this device. Keep this page open.";
    }
  }

  const updateQueueRequest = createQueueOutbox(localStorage, databaseName);

  return { writeLocalDraft, updateQueueRequest };
}

export const { writeLocalDraft, updateQueueRequest } = createLocalDraftStorage(
  serverLocalStorage,
  serverStorageName(LOCAL_DRAFT_DATABASE_NAME),
);
