import type { DraftVersion } from "./drafts";

export type DraftValueSource = Pick<Storage, "length" | "key" | "getItem">;
export type PendingDraft = { key: string; version: DraftVersion };
export type LocalDraftRecord = {
  version: 1;
  workspace: string;
  session: string;
  text: string;
  deleted: boolean;
  source: "local" | "remote";
  updated?: number;
};

export const journalPrefix = (scope: string) => `${scope}:pending:`;
export const localDraftPrefix = "codex-chat-draft:";
export const draftWorkspace = (scope: string) =>
  scope.slice("codex-drafts:".length);
export function localDraftKey(scope: string, session: string) {
  return `${localDraftPrefix}${draftWorkspace(scope)}:${encodeURIComponent(session)}`;
}

export function readSavedDraftJournal(
  storage: DraftValueSource,
  scope: string,
): PendingDraft[] {
  const entries: PendingDraft[] = [];
  for (let index = 0; index < storage.length; index++) {
    const key = storage.key(index)!;
    if (!key.startsWith(journalPrefix(scope))) continue;
    const version = JSON.parse(storage.getItem(key)!);
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

export function parseLocalDraftRecord(
  raw: string,
  workspace: string,
  session: string,
): LocalDraftRecord {
  const value = JSON.parse(raw) as Record<string, unknown>;
  if (
    !value ||
    value.version !== 1 ||
    value.workspace !== workspace ||
    value.session !== session ||
    typeof value.text !== "string" ||
    typeof value.deleted !== "boolean" ||
    (value.source !== "local" &&
      value.source !== "remote" &&
      value.source !== "legacy") ||
    (value.updated !== undefined && !Number.isFinite(value.updated))
  )
    throw new Error("A saved draft could not be read. Keep this chat open.");
  return {
    version: 1 as const,
    workspace,
    session,
    text: value.text,
    deleted: value.deleted,
    source: value.source === "legacy" ? "local" : value.source,
    ...(value.updated !== undefined
      ? { updated: value.updated as number }
      : {}),
  };
}

export function readSavedLocalDraftRecord(
  storage: Pick<Storage, "getItem">,
  scope: string,
  session: string,
) {
  const raw = storage.getItem(localDraftKey(scope, session));
  return raw === null
    ? null
    : parseLocalDraftRecord(raw, draftWorkspace(scope), session);
}
