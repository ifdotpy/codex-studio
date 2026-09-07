export type Recording = {
  id: string;
  chatId: string;
  created: number;
  sampleRate: number;
  samples: number;
  state: "recording" | "ready";
  transcript?: string;
  transcriptionAttempt?: string;
  error?: string;
};
const database = () =>
  new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open("codex-studio-dictation", 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore("recordings", { keyPath: "id" });
      request.result.createObjectStore("chunks", { keyPath: ["id", "index"] });
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
async function transaction<T>(
  stores: string[],
  mode: IDBTransactionMode,
  action: (tx: IDBTransaction) => IDBRequest<T>,
): Promise<T> {
  const db = await database();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(stores, mode);
    const request = action(tx);
    tx.oncomplete = () => {
      db.close();
      resolve(request.result);
    };
    tx.onerror = tx.onabort = () => {
      db.close();
      reject(tx.error || Error("Cannot save the recording."));
    };
  });
}
export async function listRecordings(chatId: string) {
  const all = await transaction<Recording[]>(["recordings"], "readonly", (tx) =>
    tx.objectStore("recordings").getAll(),
  );
  return all
    .filter((row) => row.chatId === chatId)
    .sort((a, b) => b.created - a.created);
}
export async function saveRecording(row: Recording) {
  await transaction(["recordings"], "readwrite", (tx) =>
    tx.objectStore("recordings").put(row),
  );
}
export async function saveChunk(
  row: Recording,
  index: number,
  pcm: Int16Array,
) {
  await transaction(["recordings", "chunks"], "readwrite", (tx) => {
    tx.objectStore("chunks").put({ id: row.id, index, pcm });
    return tx.objectStore("recordings").put(row);
  });
}
export async function deleteRecording(id: string) {
  await transaction(["recordings", "chunks"], "readwrite", (tx) => {
    tx.objectStore("chunks").delete(
      IDBKeyRange.bound([id, 0], [id, Number.MAX_SAFE_INTEGER]),
    );
    return tx.objectStore("recordings").delete(id);
  });
}
export function wav(chunks: Int16Array[], sampleRate: number) {
  const samples = chunks.reduce((sum, part) => sum + part.length, 0);
  const output = new ArrayBuffer(44 + samples * 2),
    view = new DataView(output);
  const word = (offset: number, value: string) =>
    [...value].forEach((c, i) => view.setUint8(offset + i, c.charCodeAt(0)));
  word(0, "RIFF");
  view.setUint32(4, 36 + samples * 2, true);
  word(8, "WAVE");
  word(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  word(36, "data");
  view.setUint32(40, samples * 2, true);
  let offset = 44;
  for (const chunk of chunks)
    for (const value of chunk) {
      view.setInt16(offset, value, true);
      offset += 2;
    }
  return new Blob([output], { type: "audio/wav" });
}
export async function recordingAudio(row: Recording) {
  const chunks = await transaction<{ pcm: Int16Array }[]>(
    ["chunks"],
    "readonly",
    (tx) =>
      tx
        .objectStore("chunks")
        .getAll(
          IDBKeyRange.bound([row.id, 0], [row.id, Number.MAX_SAFE_INTEGER]),
        ),
  );
  return wav(
    chunks.map((chunk) => chunk.pcm),
    row.sampleRate,
  );
}

// Late transcription responses must not recreate a deleted recording.
export async function updateRecording(row: Recording, attempt?: string) {
  await transaction(["recordings"], "readwrite", (tx) => {
    const records = tx.objectStore("recordings");
    const request = records.get(row.id);
    request.onsuccess = () => {
      if (
        request.result?.chatId === row.chatId &&
        (!attempt || request.result.transcriptionAttempt === attempt)
      )
        records.put({ ...request.result, ...row });
    };
    return request;
  });
}

export async function beginTranscription(row: Recording, attempt: string) {
  const existing = await transaction<Recording | undefined>(
    ["recordings"],
    "readwrite",
    (tx) => {
      const records = tx.objectStore("recordings");
      const request = records.get(row.id);
      request.onsuccess = () => {
        if (request.result?.chatId === row.chatId)
          records.put({ ...request.result, transcriptionAttempt: attempt });
      };
      return request;
    },
  );
  return existing?.chatId === row.chatId;
}
