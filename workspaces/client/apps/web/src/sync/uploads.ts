import { randomUUID } from "../uuid";
import { serverStorageName } from "../servers/environment";
import { serverLocalStorage as localStorage } from "../servers/storage";
import { syncGet, syncPost, refreshSession } from "../api";
import { writeLocalDraft } from "./localDraft";
import { syncDatabase } from "./client";
import { attachmentLimit, type Attachment } from "../attachmentSelection";
import { USER_ATTACHMENT_LIMIT_LABEL } from "../generated/sharedValues";

export { attachmentLimit } from "../attachmentSelection";
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
export const UPLOAD_DATABASE_NAME = "codex-studio-uploads";
export type UploadBatchIdentity = Readonly<{ id: string; created: number }>;
export function createUploadBatch(
  count: number,
): readonly UploadBatchIdentity[] {
  if (!Number.isSafeInteger(count) || count < 1)
    throw new Error("Select at least one file.");
  const created = Date.now();
  return Object.freeze(
    Array.from({ length: count }, (_, index) =>
      Object.freeze({ id: randomUUID(), created: created + index }),
    ),
  );
}
export function createUploads(
  options = {
    databaseName: serverStorageName(UPLOAD_DATABASE_NAME),
    storage: localStorage,
    syncGet,
    syncPost,
    refreshSession,
    writeLocalDraft,
    syncDatabase,
  },
  openDatabase?: () => Promise<IDBDatabase>,
) {
  const {
    databaseName,
    syncGet,
    syncPost,
    refreshSession,
    writeLocalDraft,
    syncDatabase,
  } = options;
  const localStorage = options.storage;
  const UPLOADS_CHANNEL_NAME =
    databaseName === serverStorageName(UPLOAD_DATABASE_NAME)
      ? "studio-uploads-changed"
      : `studio-uploads-changed:${databaseName}`;
  let uploadsChannel: BroadcastChannel | undefined;
  let uploadsChannelSubscribers = 0;
  function onUploadsChanged(listener: () => void) {
    window.addEventListener(UPLOADS_CHANNEL_NAME, listener);
    let channel: BroadcastChannel | undefined;
    if (typeof BroadcastChannel !== "undefined") {
      uploadsChannel ??= new BroadcastChannel(UPLOADS_CHANNEL_NAME);
      channel = uploadsChannel;
      channel.addEventListener("message", listener);
      uploadsChannelSubscribers += 1;
    }
    return () => {
      window.removeEventListener(UPLOADS_CHANNEL_NAME, listener);
      if (!channel) return;
      channel.removeEventListener("message", listener);
      uploadsChannelSubscribers -= 1;
      if (!uploadsChannelSubscribers) {
        channel.close();
        uploadsChannel = undefined;
      }
    };
  }
  const uploadsChanged = () => {
    window.dispatchEvent(new Event(UPLOADS_CHANNEL_NAME));
    if (typeof BroadcastChannel === "undefined") return;
    if (uploadsChannel) {
      uploadsChannel.postMessage({ type: "changed" });
      return;
    }
    const channel = new BroadcastChannel(UPLOADS_CHANNEL_NAME);
    channel.postMessage({ type: "changed" });
    setTimeout(() => channel.close(), 0);
  };

  const database =
    openDatabase ??
    (() =>
      new Promise<IDBDatabase>((resolve, reject) => {
        const request = indexedDB.open(databaseName, 2);
        request.onupgradeneeded = () => {
          const db = request.result;
          const uploads = db.objectStoreNames.contains("uploads")
            ? request.transaction!.objectStore("uploads")
            : db.createObjectStore("uploads", { keyPath: "id" });
          const contents = db.createObjectStore("contents", { keyPath: "id" });
          uploads.openCursor().onsuccess = (event) => {
            const cursor = (
              event.target as IDBRequest<IDBCursorWithValue | null>
            ).result;
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
      }));
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
  async function queueUploads(
    stateDir: string,
    agent: string,
    files: File[],
    expectedWorkspace: string,
    identities: readonly UploadBatchIdentity[] = createUploadBatch(
      files.length,
    ),
  ) {
    if (!files.length) throw new Error("Select at least one file.");
    for (const file of files)
      if (!file.size || file.size > attachmentLimit)
        throw new Error(
          `Files must contain 1 byte to ${USER_ATTACHMENT_LIMIT_LABEL}.`,
        );
    if (!expectedWorkspace)
      throw new Error(
        "Wait for this workspace to finish loading before attaching files.",
      );
    if (
      identities.length !== files.length ||
      new Set(identities.map((row) => row.id)).size !== files.length ||
      identities.some(
        (row) =>
          !row.id || !Number.isSafeInteger(row.created) || row.created < 0,
      )
    )
      throw new Error("The original upload identities are invalid.");
    const { workspaceId } = await syncDatabase();
    if (workspaceId !== expectedWorkspace)
      throw new Error(
        "The workspace changed. The selected files have not been uploaded.",
      );
    // Copy provider-backed File data before opening the transaction. WebKit cannot
    // reliably put those File objects in IndexedDB. ArrayBuffer retains exact bytes.
    const rows: (PendingUpload & { bytes: ArrayBuffer })[] = await Promise.all(
      files.map(async (file, index) => ({
        id: identities[index].id,
        workspace: workspaceId,
        stateDir,
        agent,
        name: file.name,
        bytes: await file.arrayBuffer(),
        size: file.size,
        mime: file.type,
        created: identities[index].created,
      })),
    );
    const db = await database();
    const retained = await new Promise<PendingUpload[]>((resolve, reject) => {
      const tx = db.transaction(["uploads", "contents"], "readwrite", {
        durability: "strict",
      });
      const store = tx.objectStore("uploads"),
        contents = tx.objectStore("contents");
      const found: Array<PendingUpload | undefined> = [],
        saved: Array<{ bytes?: ArrayBuffer } | undefined> = [];
      let pending = rows.length * 2,
        failure: Error | undefined;
      const refuse = () => {
        failure = new Error(
          "The saved upload identity has different contents or another owner.",
        );
        tx.abort();
      };
      const ready = () => {
        if (--pending) return;
        // Validate every original identity before adding any row. The transaction
        // serializes concurrent retries and preserves an already delivered asset.
        for (const [index, { bytes, ...row }] of rows.entries()) {
          const old = found[index],
            content = saved[index];
          if (uploadCancelled(row.id) || !!old !== !!content) return refuse();
          if (!old) continue;
          const original =
            content?.bytes instanceof ArrayBuffer
              ? new Uint8Array(content.bytes)
              : undefined;
          if (
            Object.entries(row).some(
              ([key, value]) => old[key as keyof PendingUpload] !== value,
            ) ||
            !original ||
            original.byteLength !== bytes.byteLength ||
            new Uint8Array(bytes).some(
              (value, offset) => value !== original[offset],
            )
          )
            return refuse();
        }
        for (const [index, { bytes, ...row }] of rows.entries()) {
          if (found[index]) continue;
          store.add(row);
          contents.add({ id: row.id, bytes });
          found[index] = row;
        }
      };
      for (const [index, row] of rows.entries()) {
        const record = store.get(row.id),
          content = contents.get(row.id);
        record.onsuccess = () => {
          found[index] = record.result;
          ready();
        };
        content.onsuccess = () => {
          saved[index] = content.result;
          ready();
        };
      }
      tx.oncomplete = () => {
        db.close();
        resolve(found as PendingUpload[]);
      };
      tx.onerror = tx.onabort = () => {
        db.close();
        reject(
          failure ??
            tx.error ??
            new Error("Files could not be saved on this device."),
        );
      };
    });
    uploadsChanged();
    return retained;
  }
  async function pendingUploads(stateDir: string, expectedWorkspace?: string) {
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
  const attachmentCancelled = (id: string) =>
    localStorage.getItem(`studio-upload-cancelled:${id}`) === "true";
  const uploadCancelled = (id: string) =>
    cancelled.has(id) ||
    attachmentCancelled(id) ||
    localStorage.getItem(`studio-upload-settled:${id}`) === "true";
  async function removeUpload(id: string, onCancelled?: () => void) {
    const error = writeLocalDraft(`studio-upload-cancelled:${id}`, true);
    if (error) throw new Error(error);
    cancelled.add(id);
    onCancelled?.();
    await finishUpload(id);
  }
  async function finishUpload(id: string) {
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
  function deliverUpload(row: PendingUpload): Promise<Attachment | null> {
    const existing = active.get(row.id);
    if (existing) return existing;
    const promise = (async () => {
      let current = await transaction<PendingUpload | undefined>(
        "readonly",
        (store) => store.get(row.id),
      );
      if (!current || uploadCancelled(row.id)) return null;
      const identity = await syncGet("/api/sync/identity");
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
        throw new Error(
          "The saved upload belongs to another workspace or chat.",
        );
      if (uploadCancelled(row.id)) return null;
      if (current.asset) return current.asset;
      const session = await refreshSession();
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
      const response = await syncPost(
        "/api/assets",
        {
          agent: row.agent,
          name: row.name,
          base64,
          id: row.id,
        },
        { workspaceId: row.workspace, sessionToken: session.token },
      );
      const asset: Attachment = response;
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

  return {
    createUploadBatch,
    uploadsChanged,
    onUploadsChanged,
    queueUploads,
    pendingUploads,
    attachmentCancelled,
    uploadCancelled,
    removeUpload,
    finishUpload,
    deliverUpload,
  };
}
export const {
  uploadsChanged,
  onUploadsChanged,
  queueUploads,
  pendingUploads,
  attachmentCancelled,
  uploadCancelled,
  removeUpload,
  finishUpload,
  deliverUpload,
} = createUploads();
