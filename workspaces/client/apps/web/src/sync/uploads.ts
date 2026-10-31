import { randomUUID } from "../uuid";
import { serverStorageName } from "../servers/environment";
import { syncGet, syncPost, refreshSession, type GetResult } from "../api";
import { syncDatabase } from "./client";
import type { Attachment } from "../components/ComposerAttachments";
import {
  MAX_USER_ATTACHMENT_BYTES,
  USER_ATTACHMENT_LIMIT_LABEL,
} from "../generated/sharedValues";

export const attachmentLimit = MAX_USER_ATTACHMENT_BYTES;
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
export type UploadOptions = {
  databaseName: string;
  syncGet: (
    path: "/api/sync/identity",
  ) => Promise<Pick<GetResult<"/api/sync/identity">, "workspaceId">>;
  syncPost: typeof syncPost<"/api/assets">;
  refreshSession: typeof refreshSession;
  syncDatabase: () => Promise<
    Pick<Awaited<ReturnType<typeof syncDatabase>>, "workspaceId">
  >;
};

export function createUploads(
  options: UploadOptions = {
    databaseName: serverStorageName(UPLOAD_DATABASE_NAME),
    syncGet,
    syncPost,
    refreshSession,
    syncDatabase,
  },
) {
  const { databaseName, syncGet, syncPost, refreshSession, syncDatabase } =
    options;
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

  const rows = new Map<string, PendingUpload>();
  const contents = new Map<string, ArrayBuffer>();
  async function queueUploads(
    stateDir: string,
    agent: string,
    files: File[],
    expectedWorkspace: string,
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
    const { workspaceId } = await syncDatabase();
    if (workspaceId !== expectedWorkspace)
      throw new Error(
        "The workspace changed. The selected files have not been uploaded.",
      );
    // Copy file data while the selection remains available.
    const queued: (PendingUpload & { bytes: ArrayBuffer })[] =
      await Promise.all(
        files.map(async (file, index) => ({
          id: randomUUID(),
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
    for (const { bytes, ...row } of queued) {
      rows.set(row.id, row);
      contents.set(row.id, bytes);
    }
    uploadsChanged();
    return queued.map(({ bytes: _bytes, ...row }) => row);
  }
  async function pendingUploads(stateDir: string, expectedWorkspace?: string) {
    const { workspaceId } = await syncDatabase();
    if (expectedWorkspace && workspaceId !== expectedWorkspace)
      throw new Error(
        "The workspace changed. Selected files remain in this page.",
      );
    return [...rows.values()]
      .filter(
        (row) =>
          row.workspace === workspaceId &&
          row.stateDir === stateDir &&
          !uploadCancelled(row.id),
      )
      .sort((a, b) => a.created - b.created || a.id.localeCompare(b.id));
  }
  const cancelled = new Set<string>();
  const attachmentCancelled = (id: string) => cancelled.has(id);
  const uploadCancelled = attachmentCancelled;
  async function removeUpload(id: string, onCancelled?: () => void) {
    cancelled.add(id);
    onCancelled?.();
    await finishUpload(id);
  }
  async function finishUpload(id: string) {
    cancelled.add(id);
    rows.delete(id);
    contents.delete(id);
    uploadsChanged();
  }
  const active = new Map<string, Promise<Attachment | null>>();
  function deliverUpload(row: PendingUpload): Promise<Attachment | null> {
    const existing = active.get(row.id);
    if (existing) return existing;
    const promise = (async () => {
      let current = rows.get(row.id);
      if (!current || uploadCancelled(row.id)) return null;
      const identity = await syncGet("/api/sync/identity");
      const storage = await syncDatabase();
      if (
        identity.workspaceId !== row.workspace ||
        storage.workspaceId !== row.workspace
      )
        throw new Error(
          "The server workspace changed. Selected files remain in this page.",
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
      const bytes = contents.get(row.id);
      if (uploadCancelled(row.id)) return null;
      if (!bytes) throw new Error("The saved file bytes are unavailable.");
      const base64 = await new Promise<string>((resolve, reject) => {
        const reader = new FileReader();
        reader.onerror = () => reject(new Error(`Cannot read ${row.name}.`));
        reader.onload = () => {
          const result = String(reader.result);
          resolve(result.slice(result.indexOf(",") + 1));
        };
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
      current = rows.get(row.id);
      if (!current || uploadCancelled(row.id)) return null;
      rows.set(row.id, { ...current, asset });
      return asset;
    })().finally(() => active.delete(row.id));
    active.set(row.id, promise);
    return promise;
  }

  return {
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
