export type JournalEdit = { id: string; changes: Record<string, string> };
export const DRAFT_JOURNAL_PREFIX = "studio-native-draft-journal-v1:";
let storage: Storage | undefined;
export function setNativeJournalStorage(value: Storage) {
  storage = value;
}
export function nativeDraftJournal(workspace: string) {
  if (!storage) throw new Error("The native recovery journal is unavailable.");
  return new DraftJournal(storage, workspace);
}

export class DraftJournal {
  private prefix: string;
  private sequence = 0;
  private storage: Storage;
  private workspace: string;
  constructor(storage: Storage, workspace: string) {
    this.storage = storage;
    this.workspace = workspace;

    this.prefix = DRAFT_JOURNAL_PREFIX + encodeURIComponent(workspace) + ":";
  }
  load(): JournalEdit[] {
    const rows: { sequence: number; edit: JournalEdit }[] = [];
    for (let i = 0; i < this.storage.length; i++) {
      const key = this.storage.key(i)!;
      if (!key.startsWith(this.prefix)) continue;
      const row = JSON.parse(this.storage.getItem(key)!);
      if (
        row?.version !== 1 ||
        row.workspace !== this.workspace ||
        !Number.isSafeInteger(row.sequence) ||
        row.sequence < 1 ||
        typeof row.edit?.id !== "string" ||
        !row.edit.id ||
        row.edit.id.length > 128 ||
        key !== this.prefix + row.edit.id ||
        !row.edit.changes ||
        typeof row.edit.changes !== "object" ||
        Array.isArray(row.edit.changes) ||
        !Object.entries(row.edit.changes).every(
          ([key, value]) => key && typeof value === "string",
        )
      )
        throw new Error(
          "Saved draft changes could not be read. Keep the recovery journal.",
        );
      this.sequence = Math.max(this.sequence, row.sequence);
      rows.push(row);
    }
    return rows
      .sort(
        (a, b) => a.sequence - b.sequence || a.edit.id.localeCompare(b.edit.id),
      )
      .map((row) => row.edit);
  }
  save(edit: JournalEdit) {
    const key = this.prefix + edit.id;
    const old = this.storage.getItem(key);
    if (old !== null) {
      const row = JSON.parse(old);
      if (
        row.workspace !== this.workspace ||
        JSON.stringify(row.edit) !== JSON.stringify(edit)
      )
        throw new Error("A saved draft request has different content.");
      return;
    }
    this.storage.setItem(
      key,
      JSON.stringify({
        version: 1,
        workspace: this.workspace,
        sequence: ++this.sequence,
        edit,
      }),
    );
  }
  acknowledge(edit: JournalEdit) {
    const key = this.prefix + edit.id;
    const raw = this.storage.getItem(key);
    if (raw === null) return;
    if (JSON.stringify(JSON.parse(raw).edit) !== JSON.stringify(edit))
      throw new Error("A saved draft request changed before acknowledgement.");
    this.storage.removeItem(key);
  }
}
