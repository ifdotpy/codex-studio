import { serverLocalStorage as localStorage } from "../servers/storage";

import {
  draftWorkspace as workspaceOf,
  localDraftPrefix as localPrefix,
  localDraftKey,
  parseLocalDraftRecord as parseRecord,
  readSavedLocalDraftRecord,
  type LocalDraftRecord,
} from "./draftRead";
export { localDraftKey, type LocalDraftRecord } from "./draftRead";

export type DraftMap = Record<string, string>;

export function createDraftStorage(localStorage: Storage) {
  function readLocalDraftRecord(scope: string, session: string) {
    return readSavedLocalDraftRecord(localStorage, scope, session);
  }

  function readLocalDrafts(
    scope: string,
    onCorrupt: () => void = () => {},
  ): DraftMap {
    const drafts: DraftMap = {};
    const workspace = workspaceOf(scope);
    const prefix = `${localPrefix}${workspace}:`;
    for (let index = 0; index < localStorage.length; index++) {
      const key = localStorage.key(index)!;
      if (!key.startsWith(prefix)) continue;
      try {
        const session = decodeURIComponent(key.slice(prefix.length));
        const record = parseRecord(
          localStorage.getItem(key)!,
          workspace,
          session,
        );
        if (!record.deleted) drafts[session] = record.text;
      } catch {
        onCorrupt();
      }
    }
    return drafts;
  }

  function writeLocalDraftRecord(
    scope: string,
    session: string,
    text: string | null,
    source: LocalDraftRecord["source"] = "local",
    metadata: Pick<LocalDraftRecord, "updated"> = {},
  ) {
    const key = localDraftKey(scope, session);
    const previousRaw = localStorage.getItem(key);
    const previous = previousRaw
      ? (() => {
          const parsed = parseRecord(previousRaw, workspaceOf(scope), session);
          const raw = JSON.parse(previousRaw) as Record<string, unknown>;
          return {
            ...Object.fromEntries(
              Object.entries(raw).filter(
                ([field]) => !field.startsWith("legacy"),
              ),
            ),
            ...parsed,
          };
        })()
      : null;
    const record: LocalDraftRecord = {
      ...previous,
      version: 1,
      workspace: workspaceOf(scope),
      session,
      text: text ?? "",
      deleted: text === null,
      source,
      ...metadata,
    };
    localStorage.setItem(key, JSON.stringify(record));
  }

  function copyLocalDraftScope(
    from: string,
    to: string,
    onCorrupt: () => void = () => {},
  ) {
    const sourceWorkspace = workspaceOf(from);
    const destinationWorkspace = workspaceOf(to);
    const prefix = `${localPrefix}${sourceWorkspace}:`;
    for (let index = 0; index < localStorage.length; index++) {
      const key = localStorage.key(index)!;
      if (!key.startsWith(prefix)) continue;
      let session: string;
      let record: LocalDraftRecord;
      try {
        session = decodeURIComponent(key.slice(prefix.length));
        record = parseRecord(
          localStorage.getItem(key)!,
          sourceWorkspace,
          session,
        );
      } catch {
        onCorrupt();
        continue;
      }
      const destination = localDraftKey(to, session);
      if (localStorage.getItem(destination) !== null) continue;
      localStorage.setItem(
        destination,
        JSON.stringify({ ...record, workspace: destinationWorkspace }),
      );
    }
  }
  return {
    readLocalDraftRecord,
    readLocalDrafts,
    writeLocalDraftRecord,
    copyLocalDraftScope,
  };
}
export const {
  readLocalDraftRecord,
  readLocalDrafts,
  writeLocalDraftRecord,
  copyLocalDraftScope,
} = createDraftStorage(localStorage);
