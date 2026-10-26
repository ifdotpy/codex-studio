import {
  DRAFT_ATTACHMENT_NAME,
  DRAFT_ATTACHMENT_MIME,
  validateAttachmentMetadata,
  type AttachmentSelections,
  type AttachmentSelectionBatch,
  type AttachmentTarget,
  type AttachmentUpload,
} from "../attachmentSelection";
import type { NativeServerContext } from "./client";
import type { NativeFileHandle } from "./bytes";
import attachmentContract from "../../../../contracts/attachment-selection.json";

export type NativeAttachmentBatch = Readonly<{
  id: string;
  enqueueRequestId: string;
  serverId: string;
  workspaceId: string;
  files: readonly NativeFileHandle[];
  target?: AttachmentTarget;
}>;
export type NativeTextAttachmentIntent = Omit<NativeAttachmentBatch, "files"> &
  Readonly<{
    stageRequestId: string;
    target: AttachmentTarget;
  }>;
/** The host owns sealed gesture receipts and pending identities, including lost replies. */
export interface NativeAttachmentHost {
  pick(context: NativeServerContext): Promise<unknown>;
  drop(context: NativeServerContext): Promise<unknown>;
  // null means native capture confirmed ordinary textarea text, not unknown capture.
  paste(context: NativeServerContext): Promise<unknown> | null;
  textIntent(
    context: NativeServerContext,
    target: AttachmentTarget,
    text: string,
  ): Promise<unknown>;
  bindTarget(
    context: NativeServerContext,
    selectionId: string,
    target: AttachmentTarget,
  ): Promise<unknown>;
  settle(
    context: NativeServerContext,
    batch: NativeAttachmentBatch,
  ): Promise<unknown>;
}
type Uploader = {
  queueUploads(
    stateDir: string,
    agent: string,
    files: readonly NativeFileHandle[],
    workspace: string,
    requestId?: string,
  ): Promise<AttachmentUpload[]>;
};
function object(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === "object" && !Array.isArray(value);
}
function sameTarget(a: AttachmentTarget, b: AttachmentTarget) {
  return (
    a.stateDir === b.stateDir &&
    a.agent === b.agent &&
    a.workspace === b.workspace
  );
}
function target(value: unknown): AttachmentTarget {
  if (
    !object(value) ||
    typeof value.stateDir !== "string" ||
    !value.stateDir ||
    typeof value.agent !== "string" ||
    !value.agent ||
    typeof value.workspace !== "string" ||
    !value.workspace
  )
    throw new Error("The attachment target is invalid.");
  return Object.freeze({
    stateDir: value.stateDir,
    agent: value.agent,
    workspace: value.workspace,
  });
}
/** No File, clipboard data, filesystem path, or byte buffer enters this adapter. */
export function createNativeAttachmentSelections(
  context: NativeServerContext,
  uploads: Uploader,
  host?: NativeAttachmentHost,
): AttachmentSelections {
  const owner = {
    serverId: context.serverId,
    workspaceId: context.workspaceId,
    handle: context.handle,
  };
  type SelectionRecord = {
    batch: NativeAttachmentBatch;
    target?: AttachmentTarget;
    bound?: boolean;
    running?: Promise<AttachmentUpload[]>;
    result?: AttachmentUpload[];
    settled?: boolean;
    textKey?: string;
  };
  type Text = {
    text: string;
    intent?: NativeTextAttachmentIntent;
    pending?: Promise<AttachmentSelectionBatch>;
  };
  const records = new WeakMap<AttachmentSelectionBatch, SelectionRecord>();
  const texts = new Map<string, Text>();
  function verify() {
    if (
      context.serverId !== owner.serverId ||
      context.workspaceId !== owner.workspaceId ||
      context.handle !== owner.handle
    )
      throw new Error("The native attachment context changed.");
  }
  function inputHost() {
    verify();
    if (!host)
      throw new Error(
        "The native host attachment gesture port is unavailable.",
      );
    return host;
  }
  function identity(value: unknown) {
    if (
      !object(value) ||
      attachmentContract.identityFields.some(
        (key) => typeof value[key] !== "string" || !value[key],
      ) ||
      value.serverId !== owner.serverId ||
      value.workspaceId !== owner.workspaceId ||
      typeof value.id !== "string" ||
      !value.id ||
      typeof value.enqueueRequestId !== "string" ||
      !value.enqueueRequestId
    )
      throw new Error(
        "The native attachment selection has another owner or identity.",
      );
    return {
      id: value.id,
      enqueueRequestId: value.enqueueRequestId,
      serverId: owner.serverId,
      workspaceId: owner.workspaceId,
    };
  }
  function adopt(value: unknown, textKey?: string): AttachmentSelectionBatch {
    verify();
    const scope = identity(value);
    if (!object(value) || !Array.isArray(value.files))
      throw new Error("The native attachment selection is invalid.");
    const files = value.files.map((file): NativeFileHandle => {
      if (
        !object(file) ||
        typeof file.nativeHandle !== "string" ||
        !file.nativeHandle ||
        typeof file.name !== "string" ||
        typeof file.type !== "string" ||
        typeof file.size !== "number"
      )
        throw new Error("The native attachment handle is invalid.");
      return Object.freeze({
        nativeHandle: file.nativeHandle,
        name: file.name,
        type: file.type,
        size: file.size,
      });
    });
    validateAttachmentMetadata(files);
    const frozenTarget =
      value.target == null ? undefined : target(value.target);
    if (frozenTarget && frozenTarget.workspace !== owner.workspaceId)
      throw new Error("The attachment target belongs to another workspace.");
    const batch = Object.freeze({
      id: scope.id,
      files: Object.freeze(
        files.map(({ name, type, size }) =>
          Object.freeze({ name, type, size }),
        ),
      ),
    });
    records.set(batch, {
      batch: Object.freeze({ ...scope, files, target: frozenTarget }),
      target: frozenTarget,
      bound: !!frozenTarget,
      textKey,
    });
    return batch;
  }
  return {
    usesInput: () => false,
    async pick() {
      const value = await inputHost().pick(context);
      return value === null ? null : adopt(value);
    },
    async input() {
      throw new Error("Select attachments through the native file picker.");
    },
    async drop(event) {
      event.preventDefault();
      const value = await inputHost().drop(context);
      return value === null ? null : adopt(value);
    },
    paste(event) {
      let value: Promise<unknown> | null;
      try {
        value = inputHost().paste(context);
      } catch (error) {
        event.preventDefault();
        throw error;
      }
      if (value === null) return null;
      event.preventDefault();
      return value.then((batch) => adopt(batch));
    },
    async text(requested, text) {
      verify();
      const frozen = target(requested);
      if (frozen.workspace !== owner.workspaceId)
        throw new Error("The attachment workspace changed.");
      const key = JSON.stringify(frozen);
      let record = texts.get(key);
      if (record && record.text !== text)
        throw new Error(
          "Resolve the original text attachment before changing its contents.",
        );
      if (!record) {
        record = { text };
        texts.set(key, record);
      }
      const saved = record;
      if (!saved.pending)
        saved.pending = (async () => {
          if (!saved.intent) {
            const value = await inputHost().textIntent(context, frozen, text);
            const scope = identity(value);
            if (
              !object(value) ||
              typeof value.stageRequestId !== "string" ||
              !value.stageRequestId ||
              !sameTarget(target(value.target), frozen)
            )
              throw new Error("The text attachment intent has another target.");
            saved.intent = Object.freeze({
              ...scope,
              stageRequestId: value.stageRequestId,
              target: frozen,
            });
          }
          verify();
          const file = await context.request(
            attachmentContract.textFile.stageMethod,
            { name: DRAFT_ATTACHMENT_NAME, mime: DRAFT_ATTACHMENT_MIME, text },
            saved.intent.stageRequestId,
          );
          return adopt({ ...saved.intent, files: [file] }, key);
        })().catch((error) => {
          saved.pending = undefined;
          throw error;
        });
      return saved.pending;
    },
    enqueue(requested, batch) {
      verify();
      const record = records.get(batch);
      if (!record)
        throw new Error(
          "The attachment selection belongs to another native owner.",
        );
      const frozen = target(requested);
      if (
        frozen.workspace !== owner.workspaceId ||
        (record.target && !sameTarget(record.target, frozen))
      )
        throw new Error(
          "The original attachment target cannot change during a retry.",
        );
      record.target ??= frozen;
      if (record.result && record.settled)
        return Promise.resolve(record.result);
      if (!record.running)
        record.running = (async () => {
          if (!record.bound) {
            const value = await inputHost().bindTarget(
              context,
              record.batch.id,
              frozen,
            );
            const bound = records.get(adopt(value))!;
            if (
              !bound.target ||
              !sameTarget(bound.target, frozen) ||
              bound.batch.id !== record.batch.id ||
              bound.batch.enqueueRequestId !== record.batch.enqueueRequestId ||
              JSON.stringify(bound.batch.files) !==
                JSON.stringify(record.batch.files)
            )
              throw new Error(
                "The committed attachment selection changed during target binding.",
              );
            record.bound = true;
          }
          verify();
          record.result ??= await uploads.queueUploads(
            frozen.stateDir,
            frozen.agent,
            record.batch.files,
            frozen.workspace,
            record.batch.enqueueRequestId,
          );
          const receipt = await inputHost().settle(context, {
            ...record.batch,
            target: frozen,
          });
          if (
            !object(receipt) ||
            receipt.selectionId !== record.batch.id ||
            receipt.enqueueRequestId !== record.batch.enqueueRequestId ||
            receipt.acknowledged !== true
          )
            throw new Error(
              "The native attachment acknowledgement does not match the original selection.",
            );
          record.settled = true;
          return record.result;
        })()
          .then((rows) => {
            record.result = rows;
            if (record.textKey) texts.delete(record.textKey);
            return rows;
          })
          .finally(() => {
            record.running = undefined;
          });
      return record.running;
    },
  };
}
