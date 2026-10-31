import { expect, it, vi } from "vitest";
import { createRecordingStorage, type Recording } from "./storage";

it("keeps recordings and audio in memory and rejects late transcription", async () => {
  const open = vi.fn(() => {
    throw new Error("must not open browser storage");
  });
  vi.stubGlobal("indexedDB", { open });
  const storage = createRecordingStorage();
  const row: Recording = {
    id: "one",
    chatId: "chat",
    created: 1,
    sampleRate: 16000,
    samples: 2,
    state: "recording",
  };
  await storage.saveChunk(row, 0, new Int16Array([1, 2]));
  expect(await storage.listRecordings("chat")).toEqual([row]);
  expect((await storage.recordingAudio(row)).size).toBe(48);
  expect(await createRecordingStorage().listRecordings("chat")).toEqual([]);
  await storage.deleteRecording(row.id);
  await storage.updateRecording({
    id: row.id,
    chatId: row.chatId,
    transcript: "late",
  });
  expect(await storage.listRecordings("chat")).toEqual([]);
  expect(open).not.toHaveBeenCalled();
  vi.unstubAllGlobals();
});
