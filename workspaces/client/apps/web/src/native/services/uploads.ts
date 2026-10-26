import contract from "../../../../../contracts/preference-storage.json";
import {
  createNativeBytes,
  type NativeFileHandle,
  type NativeUpload,
} from "../bytes";
import type { NativeServerContext } from "../client";
import type { Attachment } from "../../components/ComposerAttachments";
import type { createNativeServiceLifecycle } from "./lifecycle";
import { compareNativeRevisions, readNativeRevision } from "./revision";

type Tombstone = { cancelled: boolean; settled: boolean };
export type NativeUploadSnapshot = {
  workspaceId: string;
  entries: NativeUpload[];
  tombstones: Record<string, Tombstone>;
  revision: string;
};
function object(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
function attachment(value: unknown): Attachment | null {
  if (value === null) return null;
  if (
    !object(value) ||
    typeof value.id !== "string" ||
    !value.id ||
    typeof value.name !== "string" ||
    typeof value.mime !== "string" ||
    typeof value.image !== "boolean" ||
    typeof value.size !== "number" ||
    !Number.isSafeInteger(value.size) ||
    value.size < 0
  )
    throw new Error("The native upload returned an invalid attachment.");
  return value as unknown as Attachment;
}

/** Native storage owns bytes and tombstones. JS retains only their UI projection. */
export function createNativeUploads(
  context: NativeServerContext,
  lifecycle: ReturnType<typeof createNativeServiceLifecycle>,
  report: (error: unknown) => void,
) {
  const bytes = createNativeBytes(context);
  const listeners = new Set<() => void>();
  const active = new Map<string, Promise<Attachment | null>>();
  let snapshot: NativeUploadSnapshot | undefined;
  let initialized = false;
  let stopped = false;
  let initialization: Promise<void> | undefined;
  const uploadsChanged = () => {
    for (const listener of listeners) listener();
  };
  const row = (value: unknown): NativeUpload => {
    if (
      !object(value) ||
      value.workspace !== context.workspaceId ||
      typeof value.id !== "string" ||
      !value.id ||
      typeof value.stateDir !== "string" ||
      typeof value.agent !== "string" ||
      !value.agent ||
      typeof value.name !== "string" ||
      typeof value.size !== "number" ||
      !Number.isSafeInteger(value.size) ||
      value.size < 0 ||
      typeof value.mime !== "string" ||
      typeof value.created !== "number"
    )
      throw new Error("The upload does not belong to this native workspace.");
    return value as NativeUpload;
  };
  const apply = (value: unknown) => {
    if (
      !object(value) ||
      value.workspaceId !== context.workspaceId ||
      !Array.isArray(value.entries) ||
      !object(value.tombstones)
    )
      throw new Error("The native upload view has another owner.");
    const revision = readNativeRevision(value.revision);
    for (const entry of value.entries) row(entry);
    for (const tombstone of Object.values(value.tombstones))
      if (
        !object(tombstone) ||
        typeof tombstone.cancelled !== "boolean" ||
        typeof tombstone.settled !== "boolean"
      )
        throw new Error("The native upload tombstone is invalid.");
    if (snapshot && compareNativeRevisions(revision, snapshot.revision) < 0)
      return;
    snapshot = value as NativeUploadSnapshot;
    uploadsChanged();
  };
  const refresh = async () => {
    apply(await context.request(contract.rpc.uploadsSnapshot, {}));
  };
  const initialize = () => {
    if (stopped)
      return Promise.reject(new Error("The native upload owner is closed."));
    if (!initialization)
      initialization = (async () => {
        await refresh();
        if (stopped)
          throw new Error("The native upload owner closed before readiness.");
        if (!initialized)
          lifecycle.watch<NativeUploadSnapshot>(
            contract.topics.uploads,
            {},
            (event) => {
              if (event.error) report(new Error(event.error.message));
              else if (event.value) {
                try {
                  apply(event.value);
                } catch (error) {
                  report(error);
                }
              }
            },
          );
        initialized = true;
      })().catch((error) => {
        initialization = undefined;
        throw error;
      });
    return initialization;
  };
  const view = () => {
    if (!snapshot)
      throw new Error(
        "Wait for the native upload view before using attachments.",
      );
    return snapshot;
  };
  const settle = async (id: string, cancel: boolean) => {
    await initialize();
    let failure: unknown;
    try {
      await (cancel ? bytes.removeUpload(id) : bytes.finishUpload(id));
    } catch (error) {
      failure = error;
    }
    await refresh();
    const state = view().tombstones[id];
    if (!(cancel ? state?.cancelled : state?.settled))
      throw (
        failure ?? new Error("The native upload settlement was not confirmed.")
      );
  };
  return {
    initialize,
    uploadsChanged,
    onUploadsChanged(listener: () => void) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    attachmentCancelled: (id: string) =>
      view().tombstones[id]?.cancelled === true,
    uploadCancelled: (id: string) => {
      const value = view().tombstones[id];
      return value?.cancelled === true || value?.settled === true;
    },
    async queueUploads(
      stateDir: string,
      agent: string,
      files: readonly NativeFileHandle[],
      workspace: string,
      requestId?: string,
    ) {
      if (workspace !== context.workspaceId)
        throw new Error(
          "The upload workspace changed before the files were saved.",
        );
      await initialize();
      const values = await bytes.queueUploads(
        stateDir,
        agent,
        files,
        workspace,
        requestId,
      );
      const rows = values.map(row);
      uploadsChanged();
      return rows;
    },
    async pendingUploads(stateDir: string, workspace = context.workspaceId) {
      if (workspace !== context.workspaceId)
        throw new Error("The upload workspace does not match this screen.");
      await initialize();
      return (await bytes.pendingUploads(stateDir, workspace))
        .map(row)
        .filter((entry) => entry.stateDir === stateDir);
    },
    async removeUpload(id: string, onCancelled?: () => void) {
      await settle(id, true);
      onCancelled?.();
    },
    finishUpload: (id: string) => settle(id, false),
    deliverUpload(value: NativeUpload) {
      row(value);
      let operation = active.get(value.id);
      if (!operation) {
        operation = initialize()
          .then(() => bytes.deliverUpload(value))
          .then(attachment)
          .finally(() => {
            active.delete(value.id);
          });
        active.set(value.id, operation);
      }
      return operation;
    },
    async close() {
      stopped = true;
      listeners.clear();
      await Promise.allSettled(active.values());
    },
  };
}
