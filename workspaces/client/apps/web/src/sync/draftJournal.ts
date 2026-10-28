import type { DraftVersion } from "../generated/rustOwned";
import { decodeDraftVersion } from "./draftDecoder";

// Read-only access to the browser draft journal written before the core owned
// drafts. Only the one-time legacy import reads it.
export type PendingDraft = { key: string; version: DraftVersion };

export const journalPrefix = (scope: string) => `${scope}:pending:`;

export function readDraftJournal(
  storage: Storage,
  scope: string,
  onInvalid?: (key: string) => void,
): PendingDraft[] {
  const entries: PendingDraft[] = [];
  for (let index = 0; index < storage.length; index++) {
    const key = storage.key(index)!;
    if (!key.startsWith(journalPrefix(scope))) continue;
    try {
      const version = decodeDraftVersion(JSON.parse(storage.getItem(key)!));
      entries.push({ key, version });
    } catch (error) {
      if (!onInvalid) throw error;
      onInvalid(key);
    }
  }
  return entries.sort(
    (a, b) =>
      a.version.updated - b.version.updated || a.key.localeCompare(b.key),
  );
}
