import { syncApi, setToken } from "../api";
import { writeLocalDraft } from "./localDraft";
import { syncDatabase } from "./client";
import type { Attachment } from "../components/ComposerAttachments";

export const attachmentLimit = 20 * 1024 * 1024;
export type PendingUpload = {
  id: string;
  workspace: string;
  stateDir: string;
  agent: string;
  name: string;
  size: number;
  mime: string;
  created: number;
  asset?: Attachment;
};
export const uploadsChanged = () =>
  window.dispatchEvent(new Event("studio-uploads-changed"));
const database = () =>
  new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open("codex-studio-uploads", 2);
    request.onupgradeneeded = () => {
      const db = request.result;
      const uploads = db.objectStoreNames.contains("uploads")
        ? request.transaction!.objectStore("uploads")
        : db.createObjectStore("uploads", { keyPath: "id" });
      const contents = db.createObjectStore("contents", { keyPath: "id" });
      uploads.openCursor().onsuccess = (event) => {
        const cursor = (event.target as IDBRequest<IDBCursorWithValue | null>)
          .result;
        if (!cursor) return;
        const { bytes, file, ...row } = cursor.value;
        contents.put({ id: row.id, bytes, file });
        cursor.update({
          ...row,
          size: bytes?.byteLength ?? file?.size ?? row.size,
        });
        cursor.continue();
      };
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
async function transaction<T>(
  mode: IDBTransactionMode,
  action: (store: IDBObjectStore, tx: IDBTransaction) => IDBRequest<T>,
  stores = ["uploads"],
): Promise<T> {
  const db = await database();
  return new Promise((resolve, reject) => {
    let tx: IDBTransaction;
    try {
      tx = db.transaction(
        stores,
        mode,
        mode === "readwrite" ? { durability: "strict" } : undefined,
      );
    } catch (error) {
      db.close();
      reject(error);
      return;
    }
    const request = action(tx.objectStore(stores[0]), tx);
    tx.oncomplete = () => {
      db.close();
      resolve(request.result);
    };
    tx.onerror = tx.onabort = (event) => {
      db.close();
      reject(
        tx.error ||
          (event.target as IDBRequest)?.error ||
          new Error(
            "Files could not be saved on this device. Keep this page open.",
          ),
      );
    };
  });
}
export async function queueUploads(
  stateDir: string,
  agent: string,
  files: File[],
  expectedWorkspace: string,
) {
  if (!files.length) throw new Error("Select at least one file.");
  for (const file of files)
    if (!file.size || file.size > attachmentLimit)
      throw new Error("Files must contain 1 byte to 20 MiB.");
  if (!expectedWorkspace)
    throw new Error(
      "Wait for this workspace to finish loading before attaching files.",
    );
  const { workspaceId } = await syncDatabase();
  if (workspaceId !== expectedWorkspace)
    throw new Error(
      "The workspace changed. The selected files have not been uploaded.",
    );
  // Copy provider-backed File data before opening the transaction. WebKit cannot
  // reliably put those File objects in IndexedDB. ArrayBuffer retains exact bytes.
  const rows: (PendingUpload & { bytes: ArrayBuffer })[] = await Promise.all(
    files.map(async (file, index) => ({
      id: crypto.randomUUID(),
      workspace: workspaceId,
      stateDir,
      agent,
      name: file.name,
      bytes: await file.arrayBuffer(),
      size: file.size,
      mime: file.type,
      created: Date.now() + index,
    })),
  );
  await transaction(
    "readwrite",
    (store, tx) => {
      let request: IDBRequest = store.count();
      for (const { bytes, ...row } of rows) {
        request = store.add(row);
        tx.objectStore("contents").add({ id: row.id, bytes });
      }
      return request;
    },
    ["uploads", "contents"],
  );
  uploadsChanged();
  return rows.map(({ bytes: _bytes, ...row }) => row);
}
export async function pendingUploads(
  stateDir: string,
  expectedWorkspace?: string,
) {
  const { workspaceId } = await syncDatabase();
  if (expectedWorkspace && workspaceId !== expectedWorkspace)
    throw new Error(
      "The workspace changed. Saved uploads remain on this device.",
    );
  const rows = await transaction<PendingUpload[]>("readonly", (store) =>
    store.getAll(),
  );
  return rows
    .filter(
      (row) =>
        row.workspace === workspaceId &&
        row.stateDir === stateDir &&
        !uploadCancelled(row.id),
    )
    .sort((a, b) => a.created - b.created || a.id.localeCompare(b.id));
}
const cancelled = new Set<string>();
export const attachmentCancelled = (id: string) =>
  localStorage.getItem(`studio-upload-cancelled:${id}`) === "true";
export const uploadCancelled = (id: string) =>
  cancelled.has(id) ||
  attachmentCancelled(id) ||
  localStorage.getItem(`studio-upload-settled:${id}`) === "true";
export async function removeUpload(id: string, onCancelled?: () => void) {
  const error = writeLocalDraft(`studio-upload-cancelled:${id}`, true);
  if (error) throw new Error(error);
  cancelled.add(id);
  onCancelled?.();
  await finishUpload(id);
}
export async function finishUpload(id: string) {
  const error = writeLocalDraft(`studio-upload-settled:${id}`, true);
  if (error) throw new Error(error);
  cancelled.add(id);
  await transaction(
    "readwrite",
    (store, tx) => {
      tx.objectStore("contents").delete(id);
      return store.delete(id);
    },
    ["uploads", "contents"],
  );
  uploadsChanged();
}
const active = new Map<string, Promise<Attachment | null>>();
export function deliverUpload(row: PendingUpload): Promise<Attachment | null> {
  const existing = active.get(row.id);
  if (existing) return existing;
  const promise = (async () => {
    let current = await transaction<PendingUpload | undefined>(
      "readonly",
      (store) => store.get(row.id),
    );
    if (!current || uploadCancelled(row.id)) return null;
    const identity = await syncApi<{ workspaceId: string }>(
      "/api/sync/identity",
    );
    const storage = await syncDatabase();
    if (
      identity.workspaceId !== row.workspace ||
      storage.workspaceId !== row.workspace
    )
      throw new Error(
        "The server workspace changed. The saved files remain on this device.",
      );
    if (
      current.workspace !== row.workspace ||
      current.agent !== row.agent ||
      current.stateDir !== row.stateDir
    )
      throw new Error("The saved upload belongs to another workspace or chat.");
    if (uploadCancelled(row.id)) return null;
    if (current.asset) return current.asset;
    const session = await syncApi<{ token: string }>("/api/session");
    setToken(session.token);
    const content = await transaction<
      { bytes?: ArrayBuffer; file?: Blob } | undefined
    >("readonly", (store) => store.get(row.id), ["contents"]);
    if (!content || uploadCancelled(row.id)) return null;
    const bytes = content.bytes || (await content.file?.arrayBuffer());
    if (!bytes) throw new Error("The saved file bytes are unavailable.");
    const base64 = await new Promise<string>((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(new Error(`Cannot read ${row.name}.`));
      reader.onload = () => resolve(String(reader.result).split(",")[1]);
      reader.readAsDataURL(new Blob([bytes], { type: row.mime }));
    });
    const response = await syncApi(
      "/api/assets",
      {
        agent: row.agent,
        name: row.name,
        base64,
        id: row.id,
      },
      { workspaceId: row.workspace },
    );
    const asset: Attachment = response.asset || response;
    if (asset.id !== row.id)
      throw new Error("The upload response does not match the saved file.");
    // A removed upload can finish late. Never recreate its attachment.
    current = await transaction<PendingUpload | undefined>(
      "readonly",
      (store) => store.get(row.id),
    );
    if (!current || uploadCancelled(row.id)) return null;
    const retained = await transaction<PendingUpload | undefined>(
      "readwrite",
      (store) => {
        const request = store.get(row.id);
        request.onsuccess = () => {
          if (request.result) store.put({ ...request.result, asset });
        };
        return request;
      },
    );
    return retained && !uploadCancelled(row.id) ? asset : null;
  })().finally(() => active.delete(row.id));
  active.set(row.id, promise);
  return promise;
}
