// Read-only access to browser draft records written before the core owned drafts.
// Only the one-time legacy import reads them. Nothing writes them any more.
export type LocalDraftRecord = {
  version: 1;
  workspace: string;
  session: string;
  text: string;
  deleted: boolean;
  source: "local" | "remote";
  updated?: number;
};

const localPrefix = "codex-chat-draft:";

export function localDraftKey(scope: string, session: string) {
  return `${localPrefix}${scope.slice("codex-drafts:".length)}:${encodeURIComponent(session)}`;
}

function workspaceOf(scope: string) {
  return scope.slice("codex-drafts:".length);
}

function parseRecord(
  value: unknown,
  workspace: string,
  session: string,
): LocalDraftRecord {
  if (
    !value ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    !("version" in value) ||
    !("workspace" in value) ||
    !("session" in value) ||
    !("text" in value) ||
    !("deleted" in value) ||
    !("source" in value) ||
    value.version !== 1 ||
    value.workspace !== workspace ||
    value.session !== session ||
    typeof value.text !== "string" ||
    typeof value.deleted !== "boolean" ||
    (value.source !== "local" &&
      value.source !== "remote" &&
      value.source !== "legacy") ||
    ("updated" in value &&
      value.updated !== undefined &&
      !Number.isFinite(value.updated))
  )
    throw new Error("A saved draft could not be read. Keep this chat open.");
  return {
    version: 1 as const,
    workspace,
    session,
    text: value.text,
    deleted: value.deleted,
    source: value.source === "legacy" ? "local" : value.source,
    ...("updated" in value && value.updated !== undefined
      ? { updated: value.updated as number }
      : {}),
  };
}

export function readLocalDraftRecord(
  storage: Storage,
  scope: string,
  session: string,
): LocalDraftRecord | null {
  const raw = storage.getItem(localDraftKey(scope, session));
  return raw === null
    ? null
    : parseRecord(JSON.parse(raw), workspaceOf(scope), session);
}
