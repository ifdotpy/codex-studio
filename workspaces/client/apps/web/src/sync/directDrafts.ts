import type { components } from "../generated/api.ts";
import { resolveDraftConflict } from "./conflicts.ts";

type Document = components["schemas"]["SyncDocument"];
type PushRow = components["schemas"]["DraftPushRow"];

const input = (document: Document) => ({
  id: document.id,
  payload: document.payload,
  seq: document.seq,
  deleted: document._deleted,
});

/** One tab's server rows and unsent edits. */
export function createDirectDrafts(options: {
  post: (rows: PushRow[]) => Promise<Document[]>;
  changed: () => void;
}) {
  const masters = new Map<string, Document>();
  const pending = new Map<string, Document>();
  let sequence = -1;
  let flushing: Promise<void> | undefined;

  const apply = (rows: Document[]) => {
    for (const row of rows) {
      const previous = masters.get(row.id);
      if (!previous || row.seq >= previous.seq) masters.set(row.id, row);
    }
    options.changed();
  };
  const flush = (): Promise<void> => {
    if (sequence < 0) return Promise.resolve();
    if (flushing) return flushing;
    flushing = (async () => {
      while (pending.size && sequence >= 0) {
        const sent = [...pending.values()].slice(0, 1);
        const rows = sent.map((document) => {
          const master = masters.get(document.id);
          return {
            newDocumentState: input(document),
            ...(master ? { assumedMasterState: input(master) } : {}),
          };
        });
        const conflicts = await options.post(rows);
        const rejected = new Map(conflicts.map((row) => [row.id, row]));
        for (const [index, document] of sent.entries()) {
          const master = rejected.get(document.id);
          if (master) {
            masters.set(master.id, master);
            const local = pending.get(document.id);
            if (local) {
              const resolved = await resolveDraftConflict(master, local);
              pending.set(local.id, resolved);
            }
          } else {
            const existing = masters.get(document.id);
            const assumed = rows[index]?.assumedMasterState;
            if (
              !existing ||
              (assumed &&
                existing.payload === assumed.payload &&
                existing._deleted === assumed.deleted)
            )
              masters.set(document.id, {
                ...document,
                seq: existing?.seq ?? 0,
              });
            if (pending.get(document.id) === document)
              pending.delete(document.id);
          }
        }
        options.changed();
      }
    })().finally(() => {
      flushing = undefined;
    });
    return flushing;
  };
  return {
    snapshot(rows: Document[], _complete: boolean, revision: number) {
      masters.clear();
      sequence = revision;
      apply(rows);
    },
    changes(rows: Document[], revision: number) {
      sequence = revision;
      apply(rows);
    },
    sequence: () => sequence,
    waitForBaseline() {
      sequence = -1;
    },
    edit(row: Document) {
      pending.set(row.id, row);
    },
    records: () =>
      [...new Map([...masters, ...pending]).values()].filter(
        (row) => !row._deleted,
      ),
    flush,
  };
}
