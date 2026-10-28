import type { nativeRequest } from "./client";
import { decodeBase64, encodeBase64 } from "./base64";
import { retriedRequest } from "./retry";
import { importLegacyRecordings } from "./legacyRecordings";
import { DICTATION_SAMPLE_RATE, type Recording } from "../dictation/storage";

/** Largest decoded WAV slice per `dictation.read`. Core limit: dictation.rs MAX_READ_BYTES. */
export const RECORDING_READ_BYTES = 256 * 1024;

// The core owns recording rows, PCM chunks, attempts and undo. The page sends commands only.
export function createNativeRecordings(
  request: typeof nativeRequest,
  legacy: () => Promise<void> = () => importLegacyRecordings(request),
) {
  const call = <T>(method: string, params: unknown) =>
    request<T>(method, params);
  let imported: Promise<void> | undefined;
  const ready = () =>
    (imported ??= legacy().catch((error) => {
      imported = undefined;
      throw error;
    }));
  return {
    async listRecordings(chatId: string) {
      await ready();
      return call<Recording[]>("dictation.list", { chatId });
    },
    createRecording(chatId: string) {
      return retriedRequest<Recording>(request, "dictation.create", {
        chatId,
        sampleRate: DICTATION_SAMPLE_RATE,
      });
    },
    saveRecording(row: Recording) {
      return call("dictation.save", { row });
    },
    saveChunk(row: Recording, index: number, pcm: Int16Array) {
      return call("dictation.chunk", {
        row,
        index,
        base64: encodeBase64(
          new Uint8Array(pcm.buffer, pcm.byteOffset, pcm.byteLength),
        ),
      });
    },
    deleteRecording(id: string) {
      return call("dictation.delete", { id });
    },
    // The core clears the attempt, so a transcript that arrives late is refused.
    trashRecording(row: Recording) {
      return call("dictation.update", {
        row: {
          id: row.id,
          chatId: row.chatId,
          deletedAt: Date.now(),
          transcriptionAttempt: null,
        },
      });
    },
    restoreRecording(row: Recording) {
      return call("dictation.update", {
        row: { id: row.id, chatId: row.chatId, deletedAt: null },
      });
    },
    async deletedRecordings(chatId: string) {
      await ready();
      return call<Recording[]>("dictation.deleted", { chatId });
    },
    async recordingAudio(row: Recording) {
      const parts: Uint8Array<ArrayBuffer>[] = [];
      let offset = 0;
      let total = Infinity;
      while (offset < total) {
        const slice = await call<{ base64: string; total: number }>(
          "dictation.read",
          { id: row.id, offset, length: RECORDING_READ_BYTES },
        );
        const bytes = decodeBase64(slice.base64);
        if (!bytes.length)
          throw new Error("The saved recording is incomplete.");
        parts.push(bytes);
        offset += bytes.length;
        total = slice.total;
      }
      return new Blob(parts, { type: "audio/wav" });
    },
    updateRecording(
      row: Partial<Recording> & Pick<Recording, "id" | "chatId">,
      attempt?: string,
    ) {
      return call("dictation.update", { row, attempt: attempt ?? null });
    },
    // The core makes the attempt identity. A retried claim replays the same attempt.
    beginTranscription(row: Recording) {
      return retriedRequest<string | null>(request, "dictation.begin", {
        id: row.id,
        chatId: row.chatId,
      });
    },
  };
}
