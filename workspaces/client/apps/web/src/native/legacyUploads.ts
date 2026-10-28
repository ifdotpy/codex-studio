import type { nativeRequest } from "./client";
import { encodeBase64 } from "./base64";
import { retriedRequest } from "./retry";
import { openLegacyDatabase, readLegacyStore } from "./legacyDatabase";
import { storageNameForServer } from "../servers/environment";
import { UPLOAD_DATABASE_NAME, type PendingUpload } from "../sync/uploads";
import { UPLOAD_PART_BYTES } from "./uploadParts";

export type LegacyUpload = PendingUpload;
export type LegacyUploadSource = {
  list(): Promise<LegacyUpload[]>;
  bytes(id: string): Promise<Blob>;
};

// Each file moves once. The core identity is the old row ID, so a repeated run
// finds the row (or its tombstone) and stages nothing. A failed file stops the
// import with its name. The old row stays in browser storage.
export async function importLegacyUploads(
  request: typeof nativeRequest,
  source: LegacyUploadSource = openLegacyUploads(),
) {
  for (const row of await source.list()) {
    if ((await request("uploads.get", { id: row.id })) !== null) continue;
    const status = await request<{ settled: boolean }>("uploads.status", {
      id: row.id,
    });
    if (status.settled) continue;
    try {
      await moveFile(request, row, await source.bytes(row.id));
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      throw new Error(
        `${row.name} could not be moved to the desktop store. It stays in browser storage. ${reason}`,
      );
    }
  }
}

async function moveFile(
  request: typeof nativeRequest,
  row: LegacyUpload,
  blob: Blob,
) {
  if (blob.size !== row.size)
    throw new Error("The saved file size does not match its record.");
  const { nativeHandle } = await retriedRequest<{ nativeHandle: string }>(
    request,
    "files.begin",
    { name: row.name, mime: row.mime, size: row.size },
    `browser-upload:${row.id}:begin`,
  );
  for (let offset = 0; offset < blob.size; offset += UPLOAD_PART_BYTES) {
    const part = new Uint8Array(
      await blob.slice(offset, offset + UPLOAD_PART_BYTES).arrayBuffer(),
    );
    await retriedRequest(
      request,
      "files.append",
      { nativeHandle, offset, base64: encodeBase64(part) },
      `browser-upload:${row.id}:${offset}`,
    );
  }
  await request("uploads.import", {
    id: row.id,
    workspace: row.workspace,
    stateDir: row.stateDir,
    agent: row.agent,
    nativeHandle,
    created: row.created,
    ...(row.asset === undefined ? {} : { asset: row.asset }),
  });
}

export function openLegacyUploads(): LegacyUploadSource {
  const name = storageNameForServer("local", UPLOAD_DATABASE_NAME);
  return {
    async list() {
      const db = await openLegacyDatabase(name);
      if (!db) return [];
      try {
        return await readLegacyStore<LegacyUpload[]>(db, "uploads", (store) =>
          store.getAll(),
        );
      } finally {
        db.close();
      }
    },
    async bytes(id) {
      const db = await openLegacyDatabase(name);
      if (!db) throw new Error("The saved file bytes are unavailable.");
      try {
        const content = await readLegacyStore<
          { bytes?: ArrayBuffer; file?: Blob } | undefined
        >(db, "contents", (store) => store.get(id));
        const blob = content?.bytes ? new Blob([content.bytes]) : content?.file;
        if (!blob) throw new Error("The saved file bytes are unavailable.");
        return blob;
      } finally {
        db.close();
      }
    },
  };
}
