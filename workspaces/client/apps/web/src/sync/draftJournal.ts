import { serverLocalStorage as localStorage } from "../servers/storage";

import { encodeDraftPayload } from "./draftPayload";
import { decodeDraftVersion } from "./draftDecoder";

import type { DraftVersion } from "./drafts";

export type PendingDraft = { key: string; version: DraftVersion };

export const journalPrefix = (scope: string) => `${scope}:pending:`;
export function createDraftJournal(localStorage: Storage) {
  function readDraftJournal(scope: string): PendingDraft[] {
    const entries: PendingDraft[] = [];
    for (let index = 0; index < localStorage.length; index++) {
      const key = localStorage.key(index)!;
      if (!key.startsWith(journalPrefix(scope))) continue;
      const version = decodeDraftVersion(
        JSON.parse(localStorage.getItem(key)!),
      );
      entries.push({ key, version });
    }
    return entries.sort(
      (a, b) =>
        a.version.updated - b.version.updated || a.key.localeCompare(b.key),
    );
  }

  function writeDraftJournal(entry: PendingDraft) {
    localStorage.setItem(entry.key, encodeDraftPayload(entry.version));
  }

  function forgetDraftJournal(entry: PendingDraft) {
    const raw = localStorage.getItem(entry.key);
    if (!raw) return;
    const saved = JSON.parse(raw);
    // A later edit can arrive while its predecessor is being committed.
    if (
      saved.updated < entry.version.updated ||
      raw === encodeDraftPayload(entry.version)
    )
      localStorage.removeItem(entry.key);
  }
  return { readDraftJournal, writeDraftJournal, forgetDraftJournal };
}
export const { readDraftJournal, writeDraftJournal, forgetDraftJournal } =
  createDraftJournal(localStorage);
