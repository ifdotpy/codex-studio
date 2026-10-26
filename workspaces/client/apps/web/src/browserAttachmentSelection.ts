import { randomUUID } from "./uuid";
import * as defaultUploads from "./sync/uploads";
import * as defaultDialogs from "./native/fileDialogs";
import {
  DRAFT_ATTACHMENT_NAME,
  DRAFT_ATTACHMENT_MIME,
  validateAttachmentMetadata,
  type AttachmentSelections,
  type AttachmentSelectionBatch,
  type AttachmentTarget,
  type AttachmentUpload,
} from "./attachmentSelection";

type Options = {
  uploads: Pick<typeof defaultUploads, "queueUploads" | "createUploadBatch">;
  fileDialogs: Pick<typeof defaultDialogs, "canPickFiles" | "pickFiles">;
};
/** Browser byte objects remain private to this browser adapter. */
export function createBrowserAttachmentSelections(
  options: Options,
): AttachmentSelections {
  type Selection = {
    files: File[];
    target?: string;
    identities?: ReturnType<Options["uploads"]["createUploadBatch"]>;
    pending?: Promise<AttachmentUpload[]>;
    result?: AttachmentUpload[];
    textKey?: string;
  };
  const selections = new WeakMap<AttachmentSelectionBatch, Selection>();
  const texts = new Map<
    string,
    { text: string; batch: AttachmentSelectionBatch }
  >();
  function select(files: File[], textKey?: string) {
    const metadata = files.map(({ name, type, size }) =>
      Object.freeze({ name, type, size }),
    );
    validateAttachmentMetadata(metadata);
    const batch = Object.freeze({
      id: randomUUID(),
      files: Object.freeze(metadata),
    });
    selections.set(batch, { files: [...files], textKey });
    return batch;
  }
  return {
    usesInput: () => !options.fileDialogs.canPickFiles(),
    async pick() {
      const files = await options.fileDialogs.pickFiles();
      return files.length ? select(files) : null;
    },
    async input(element) {
      const files = Array.from(element.files || []);
      return files.length ? select(files) : null;
    },
    async drop(event) {
      event.preventDefault();
      const files = Array.from(event.dataTransfer.files);
      return files.length ? select(files) : null;
    },
    paste(event) {
      const files = Array.from(event.clipboardData.files);
      if (!files.length) return null;
      event.preventDefault();
      return Promise.resolve(select(files));
    },
    async text(target, text) {
      const key = JSON.stringify([
        target.stateDir,
        target.agent,
        target.workspace,
      ]);
      const existing = texts.get(key);
      if (existing && existing.text !== text)
        throw new Error(
          "Resolve the original text attachment before changing its contents.",
        );
      if (existing) return existing.batch;
      const batch = select(
        [
          new File([text], DRAFT_ATTACHMENT_NAME, {
            type: DRAFT_ATTACHMENT_MIME,
          }),
        ],
        key,
      );
      texts.set(key, { text, batch });
      return batch;
    },
    enqueue(target: AttachmentTarget, batch) {
      const selected = selections.get(batch);
      if (!selected)
        throw new Error(
          "The attachment selection belongs to another browser owner.",
        );
      const owner = JSON.stringify([
        target.stateDir,
        target.agent,
        target.workspace,
      ]);
      if (selected.target && selected.target !== owner)
        throw new Error(
          "The original attachment target cannot change during a retry.",
        );
      selected.target ??= owner;
      selected.identities ??= options.uploads.createUploadBatch(
        selected.files.length,
      );
      if (selected.result) return Promise.resolve(selected.result);
      if (!selected.pending)
        selected.pending = options.uploads
          .queueUploads(
            target.stateDir,
            target.agent,
            selected.files,
            target.workspace,
            selected.identities,
          )
          .then((rows) => {
            selected.result = rows;
            if (selected.textKey) texts.delete(selected.textKey);
            return rows;
          })
          .finally(() => {
            selected.pending = undefined;
          });
      return selected.pending;
    },
  };
}
export const defaultAttachmentSelections = createBrowserAttachmentSelections({
  uploads: defaultUploads,
  fileDialogs: defaultDialogs,
});
