import type { ClipboardEvent, DragEvent } from "react";
import attachmentContract from "../../../contracts/attachment-selection.json";
import {
  MAX_TURN_ATTACHMENTS,
  MAX_USER_ATTACHMENT_BYTES,
  USER_ATTACHMENT_LIMIT_LABEL,
} from "./generated/sharedValues";

export interface Attachment {
  id: string;
  name: string;
  mime: string;
  image: boolean;
  size: number;
  preview?: string;
}
export type AttachmentFileMetadata = Readonly<{
  name: string;
  type: string;
  size: number;
}>;
export type AttachmentSelectionBatch = Readonly<{
  id: string;
  files: readonly AttachmentFileMetadata[];
}>;
export type AttachmentTarget = Readonly<{
  stateDir: string;
  agent: string;
  workspace: string;
}>;
export type AttachmentUpload = {
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
export const DRAFT_ATTACHMENT_NAME = attachmentContract.textFile.name;
export const DRAFT_ATTACHMENT_MIME = attachmentContract.textFile.mime;
export const attachmentLimit = MAX_USER_ATTACHMENT_BYTES;

export function validateAttachmentMetadata(
  files: readonly AttachmentFileMetadata[],
) {
  if (!files.length) throw new Error("Select at least one file.");
  if (files.length > MAX_TURN_ATTACHMENTS)
    throw new Error(`Attach at most ${MAX_TURN_ATTACHMENTS} files.`);
  for (const file of files) {
    if (
      !file ||
      typeof file.name !== "string" ||
      !file.name ||
      typeof file.type !== "string" ||
      !Number.isSafeInteger(file.size) ||
      file.size < 1 ||
      file.size > attachmentLimit
    )
      throw new Error(
        `Files must contain 1 byte to ${USER_ATTACHMENT_LIMIT_LABEL}.`,
      );
  }
}
/** Shared UI carries metadata only. Each platform keeps its byte owner private. */
export interface AttachmentSelections {
  usesInput(): boolean;
  pick(): Promise<AttachmentSelectionBatch | null>;
  input(element: HTMLInputElement): Promise<AttachmentSelectionBatch | null>;
  drop(event: DragEvent): Promise<AttachmentSelectionBatch | null>;
  paste(event: ClipboardEvent): Promise<AttachmentSelectionBatch | null> | null;
  text(
    target: AttachmentTarget,
    text: string,
  ): Promise<AttachmentSelectionBatch>;
  enqueue(
    target: AttachmentTarget,
    batch: AttachmentSelectionBatch,
  ): Promise<AttachmentUpload[]>;
}
