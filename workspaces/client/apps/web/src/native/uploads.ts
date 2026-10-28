import type { nativeRequest } from "./client";
import { encodeBase64 } from "./base64";
import { retriedRequest } from "./retry";
import { importLegacyUploads } from "./legacyUploads";
import { UPLOAD_PART_BYTES } from "./uploadParts";
import type { Attachment } from "../components/ComposerAttachments";
import type { PendingUpload } from "../sync/uploads";
import {
  MAX_USER_ATTACHMENT_BYTES,
  USER_ATTACHMENT_LIMIT_LABEL,
} from "../generated/sharedValues";

export { UPLOAD_PART_BYTES } from "./uploadParts";

// The core owns staged bytes, queued rows, cancellation and delivery identity.
// This service keeps only the cancel set of the current page.
export function createNativeUploads(
  request: typeof nativeRequest,
  legacy: () => Promise<void> = () => importLegacyUploads(request),
) {
  const cancelled = new Set<string>();
  const active = new Map<string, Promise<Attachment | null>>();
  let imported: Promise<void> | undefined;
  const ready = () =>
    (imported ??= legacy().catch((error) => {
      imported = undefined;
      throw error;
    }));
  const listeners = new Set<() => void>();
  const uploadCancelled = (id: string) => cancelled.has(id);
  const uploadsChanged = () => {
    for (const listener of listeners) listener();
  };
  function onUploadsChanged(listener: () => void) {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }
  async function queueUploads(
    stateDir: string,
    agent: string,
    files: File[],
    expectedWorkspace: string,
  ) {
    if (!files.length) throw new Error("Select at least one file.");
    for (const file of files)
      if (!file.size || file.size > MAX_USER_ATTACHMENT_BYTES)
        throw new Error(
          `Files must contain 1 byte to ${USER_ATTACHMENT_LIMIT_LABEL}.`,
        );
    if (!expectedWorkspace)
      throw new Error(
        "Wait for this workspace to finish loading before attaching files.",
      );
    const handles: { nativeHandle: string }[] = [];
    for (const file of files) {
      const { nativeHandle } = await retriedRequest<{ nativeHandle: string }>(
        request,
        "files.begin",
        { name: file.name, mime: file.type, size: file.size },
      );
      for (let offset = 0; offset < file.size; offset += UPLOAD_PART_BYTES) {
        const part = new Uint8Array(
          await file.slice(offset, offset + UPLOAD_PART_BYTES).arrayBuffer(),
        );
        await retriedRequest(request, "files.append", {
          nativeHandle,
          offset,
          base64: encodeBase64(part),
        });
      }
      handles.push({ nativeHandle });
    }
    const rows = await retriedRequest<PendingUpload[]>(
      request,
      "uploads.enqueue",
      { workspace: expectedWorkspace, stateDir, agent, files: handles },
    );
    uploadsChanged();
    return rows;
  }
  async function pendingUploads(stateDir: string, expectedWorkspace?: string) {
    await ready();
    const rows = await request<PendingUpload[]>("uploads.pending", {
      stateDir,
      workspace: expectedWorkspace ?? "",
    });
    return rows.filter((row) => !cancelled.has(row.id));
  }
  async function removeUpload(id: string, onCancelled?: () => void) {
    cancelled.add(id);
    onCancelled?.();
    try {
      await retriedRequest(request, "uploads.cancel", { id });
    } catch (error) {
      cancelled.delete(id);
      throw error;
    }
    uploadsChanged();
  }
  async function finishUpload(id: string) {
    await retriedRequest(request, "uploads.finish", { id });
    cancelled.add(id);
    uploadsChanged();
  }
  function deliverUpload(row: PendingUpload): Promise<Attachment | null> {
    const existing = active.get(row.id);
    if (existing) return existing;
    const promise = (async () => {
      if (cancelled.has(row.id)) return null;
      const asset = await request<Attachment | null>("uploads.deliver", {
        row,
      });
      // A cancel can land while the core waits for the server. Drop that reply.
      return cancelled.has(row.id) ? null : asset;
    })().finally(() => active.delete(row.id));
    active.set(row.id, promise);
    return promise;
  }

  return {
    uploadsChanged,
    onUploadsChanged,
    queueUploads,
    pendingUploads,
    attachmentCancelled: uploadCancelled,
    uploadCancelled,
    removeUpload,
    finishUpload,
    deliverUpload,
  };
}
