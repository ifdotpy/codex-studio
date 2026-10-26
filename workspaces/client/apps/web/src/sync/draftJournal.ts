import { serverLocalStorage as localStorage } from "../servers/storage";

import { encodeDraftPayload } from "./draftPayload";

import { readSavedDraftJournal, type PendingDraft } from "./draftRead";
export { journalPrefix, type PendingDraft } from "./draftRead";
export function createDraftJournal(localStorage: Storage) {
  function readDraftJournal(scope: string): PendingDraft[] {
    return readSavedDraftJournal(localStorage, scope);
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
