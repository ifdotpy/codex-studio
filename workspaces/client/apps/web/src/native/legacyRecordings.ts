import type { nativeRequest } from "./client";
import { encodeBase64 } from "./base64";
import { openLegacyDatabase, readLegacyStore } from "./legacyDatabase";
import { storageNameForServer } from "../servers/environment";
import { DICTATION_DATABASE_NAME, type Recording } from "../dictation/storage";

export type LegacyRecordingSource = {
  list(): Promise<Recording[]>;
  chunks(id: string): AsyncIterable<{ index: number; pcm: Int16Array }>;
};

// A recording moves once. The core marks each finished import, so a deleted
// recording never returns. A recording that fails becomes an error row that
// the user can delete. Its original stays in browser storage.
export async function importLegacyRecordings(
  request: typeof nativeRequest,
  source: LegacyRecordingSource = openLegacyRecordings(),
) {
  for (const row of await source.list()) {
    const { done } = await request<{ done: boolean }>("dictation.import", {
      row,
    });
    if (done) continue;
    try {
      for await (const chunk of source.chunks(row.id)) {
        await request("dictation.chunk", {
          row,
          index: chunk.index,
          base64: encodeBase64(
            new Uint8Array(
              chunk.pcm.buffer,
              chunk.pcm.byteOffset,
              chunk.pcm.byteLength,
            ),
          ),
        });
      }
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      await request("dictation.delete", { id: row.id });
      await request("dictation.save", {
        row: {
          ...row,
          samples: 0,
          state: "ready",
          error: `Could not move this recording from browser storage: ${reason}. The original stays in browser storage.`,
        },
      });
    }
    await request("dictation.imported", { id: row.id });
  }
}

/** The old browser rows of one server ("local" or a paired server id). */
export function openLegacyRecordings(server = "local"): LegacyRecordingSource {
  const name = storageNameForServer(server, DICTATION_DATABASE_NAME);
  return {
    async list() {
      const db = await openLegacyDatabase(name);
      if (!db) return [];
      try {
        return await readLegacyStore<Recording[]>(db, "recordings", (store) =>
          store.getAll(),
        );
      } finally {
        db.close();
      }
    },
    // Reads one chunk per transaction, so a long recording never sits in memory whole.
    async *chunks(id) {
      const db = await openLegacyDatabase(name);
      if (!db) return;
      try {
        const range = IDBKeyRange.bound([id, 0], [id, Number.MAX_SAFE_INTEGER]);
        const keys = await readLegacyStore<IDBValidKey[]>(
          db,
          "chunks",
          (store) => store.getAllKeys(range),
        );
        for (const key of keys) {
          const chunk = await readLegacyStore<{
            index: number;
            pcm: Int16Array;
          }>(db, "chunks", (store) => store.get(key));
          yield { index: chunk.index, pcm: chunk.pcm };
        }
      } finally {
        db.close();
      }
    },
  };
}
