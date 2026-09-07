import { encodeDraftPayload } from "./draftPayload";
import type { DraftVersion } from "./drafts";

export type PendingDraft = { key: string; version: DraftVersion };
export const journalPrefix = (scope: string) => `${scope}:pending:`;
export function readDraftJournal(scope: string): PendingDraft[] {
  const entries: PendingDraft[] = [];
  for (let index = 0; index < localStorage.length; index++) {
    const key = localStorage.key(index)!;
    if (!key.startsWith(journalPrefix(scope))) continue;
    const version = JSON.parse(localStorage.getItem(key)!);
    if (
      !version ||
      typeof version.id !== "string" ||
      typeof version.session !== "string" ||
      typeof version.device !== "string" ||
      typeof version.text !== "string" ||
      !Number.isFinite(version.updated)
    )
      throw new Error("A saved draft could not be read. Keep this chat open.");
    entries.push({ key, version });
  }
  return entries.sort(
    (a, b) =>
      a.version.updated - b.version.updated || a.key.localeCompare(b.key),
  );
}
export function writeDraftJournal(entry: PendingDraft) {
  localStorage.setItem(entry.key, encodeDraftPayload(entry.version));
}
export function forgetDraftJournal(entry: PendingDraft) {
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
