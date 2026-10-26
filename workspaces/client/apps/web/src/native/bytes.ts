import type { NativeServerContext } from "./client";
import type { AttachmentUpload } from "../attachmentSelection";

export type NativeFileHandle = Readonly<{
  nativeHandle: string;
  name: string;
  type: string;
  size: number;
}>;
export type NativeUpload = AttachmentUpload;
export type NativeRecording = {
  id: string;
  chatId: string;
  created: number;
  sampleRate: number;
  samples: number;
  state: "recording" | "ready";
  captureRequestId?: string;
  transcript?: string;
  reviewedText?: string;
  transcriptionAttempt?: string;
  error?: string;
  deletedAt?: number;
};

/** The context, not the selected chat, owns each file and recording handle. */
export function createNativeBytes(context: NativeServerContext) {
  const request = context.request;
  return {
    speechLanguages: () =>
      request<{
        locale: string;
        choices: Array<{ code: string; label: string }>;
      }>("speech.languages", {}),
    pickAttachments: () => request<NativeFileHandle[]>("desktop.pickFiles", {}),
    queueUploads: (
      stateDir: string,
      agent: string,
      files: readonly NativeFileHandle[],
      workspace: string,
      requestId?: string,
    ) =>
      request<NativeUpload[]>(
        "uploads.enqueue",
        {
          stateDir,
          agent,
          files,
          workspace,
        },
        requestId,
      ),
    pendingUploads: (stateDir: string, workspace = context.workspaceId) =>
      request<NativeUpload[]>("uploads.pending", { stateDir, workspace }),
    deliverUpload: (row: NativeUpload) =>
      request<NativeUpload["asset"] | null>("uploads.deliver", { row }, row.id),
    uploadStatus: (id: string) =>
      request<{ cancelled: boolean; settled: boolean }>("uploads.status", {
        id,
      }),
    removeUpload: (id: string) =>
      request("uploads.cancel", { id }, `upload-cancel:${id}`),
    finishUpload: (id: string) =>
      request("uploads.finish", { id }, `upload-finish:${id}`),
    listRecordings: (chatId: string) =>
      request<NativeRecording[]>("dictation.list", { chatId }),
    deletedRecordings: (chatId: string) =>
      request<NativeRecording[]>("dictation.deleted", { chatId }),
    getRecording: (id: string, chatId: string) =>
      request<NativeRecording>("dictation.get", { id, chatId }),
    trashRecording: (row: NativeRecording) =>
      request("dictation.trash", { id: row.id, chatId: row.chatId }),
    restoreRecording: (row: NativeRecording) =>
      request("dictation.restore", { id: row.id, chatId: row.chatId }),
    updateRecording: (
      row: Pick<NativeRecording, "id" | "chatId"> & Partial<NativeRecording>,
    ) => request("dictation.update", { row }),
    requestMicrophone: () => request("desktop.requestMicrophone", {}),
    startRecording: (chatId: string, requestId: string) =>
      request<NativeRecording>("media.recordingStart", { chatId }, requestId),
    recordingState: (requestId: string) =>
      request<NativeRecording | null>("media.recordingState", { requestId }),
    stopRecording: (recording: string) =>
      request<NativeRecording>("media.recordingStop", { recording }),
    prepareTranscription: () =>
      request<string>("desktop.prepareTranscription", {}),
    // A lost response never starts a second transcription. Read the saved row.
    transcribeRecording: (
      row: NativeRecording,
      permit: string,
      locale: string,
      attempt: string,
    ) =>
      request<{ text: string }>(
        "speech.transcribe",
        { recording: row.id, chatId: row.chatId, permit, locale, id: attempt },
        attempt,
      ),
    cancelTranscription: (id: string) => request("speech.cancel", { id }),
    playRecording: (recording: string) =>
      request("media.playRecording", { recording }),
    stopPlayback: () => request("media.stopPlayback", {}),
    saveRecording: (recording: string) =>
      request("desktop.saveRecording", { value: recording }),
  };
}

/** Migration only reads browser-owned source data. It never removes that source. */
export function createNativeByteImport(context: NativeServerContext) {
  return {
    begin: (manifest: unknown) =>
      context.request<{
        manifestId: string;
        manifestHash: string;
        chunkBytes: number;
        status: "pending" | "complete";
      }>("files.importBegin", { manifest }),
    chunk: (
      manifestId: string,
      entry: number,
      offset: number,
      base64: string,
    ) =>
      context.request<{ nextOffset: number }>("files.importChunk", {
        manifestId,
        entry,
        offset,
        base64,
      }),
    commit: (manifestId: string) =>
      context.request("files.importCommit", { manifestId }),
    readback: (manifestId: string) =>
      context.request("files.importReadback", { manifestId }),
    hashBegin: (id: string, bytes: number) =>
      context.request<{
        chunkBytes: number;
        bytes: number;
        sha256: string | null;
      }>("files.hashBegin", { id, bytes }),
    hashChunk: (id: string, offset: number, base64: string) =>
      context.request("files.hashChunk", { id, offset, base64 }),
    hashFinish: (id: string) =>
      context.request<{ bytes: number; sha256: string }>("files.hashFinish", {
        id,
      }),
  };
}
