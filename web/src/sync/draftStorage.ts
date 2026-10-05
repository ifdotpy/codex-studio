export type LocalDraftRecord = {
  version: 1;
  workspace: string;
  session: string;
  text: string;
  deleted: boolean;
  source: "local" | "remote";
  updated?: number;
};

export type DraftMap = Record<string, string>;

const localPrefix = "codex-chat-draft:";

export function localDraftKey(scope: string, session: string) {
  return `${localPrefix}${scope.slice("codex-drafts:".length)}:${encodeURIComponent(session)}`;
}

function workspaceOf(scope: string) {
  return scope.slice("codex-drafts:".length);
}

function parseRecord(
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

export function readLocalDraftRecord(scope: string, session: string) {
  const raw = localStorage.getItem(localDraftKey(scope, session));
  return raw === null ? null : parseRecord(raw, workspaceOf(scope), session);
}

export function readLocalDrafts(
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

export function writeLocalDraftRecord(
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

export function copyLocalDraftScope(
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
