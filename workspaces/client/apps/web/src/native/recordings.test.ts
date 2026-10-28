import { expect, it } from "vitest";
import { createNativeRecordings, RECORDING_READ_BYTES } from "./recordings";
import { DICTATION_SAMPLE_RATE, type Recording } from "../dictation/storage";
import {
  importLegacyRecordings,
  type LegacyRecordingSource,
} from "./legacyRecordings";
import { readFileSync } from "node:fs";

const row: Recording = {
  id: "rec-1",
  chatId: "chat-1",
  created: 1,
  sampleRate: 16000,
  samples: 2,
  state: "ready",
};
// Test doubles receive untrusted JSON-like params; a record check narrows them.
function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : {};
}

function fake(
  replies: Record<string, (params: Record<string, unknown>) => unknown>,
  lost: string[] = [],
) {
  const calls: {
    method: string;
    params: Record<string, unknown>;
    id?: string | undefined;
  }[] = [];
  const request = (async (method: string, params: unknown, id?: string) => {
    calls.push({ method, params: record(params), id });
    if (lost.includes(method)) {
      lost.splice(lost.indexOf(method), 1);
      throw new Error("The native client did not answer.");
    }
    const reply = replies[method];
    if (!reply) throw new Error(`no reply for ${method}`);
    return reply(record(params));
  }) as Parameters<typeof createNativeRecordings>[0];
  return { calls, request };
}
const noLegacy = async () => {};

it("creates a recording with the identity from the core, and retries a lost reply", async () => {
  const core = fake(
    {
      "dictation.create": () => ({
        ...row,
        id: "core-1",
        samples: 0,
        state: "recording",
      }),
    },
    ["dictation.create"],
  );
  const storage = createNativeRecordings(core.request, noLegacy);
  const created = await storage.createRecording("chat-1");
  expect(created.id).toBe("core-1");
  const calls = core.calls.filter((call) => call.method === "dictation.create");
  expect(calls.map((call) => call.params)).toEqual([
    { chatId: "chat-1", sampleRate: DICTATION_SAMPLE_RATE },
    { chatId: "chat-1", sampleRate: DICTATION_SAMPLE_RATE },
  ]);
  expect(calls[0]?.id).toBe(calls[1]?.id);
});

it("joins the WAV slices the core returns", async () => {
  const pcm = new Uint8Array([1, 0, 2, 0]);
  const wav = new Uint8Array(44 + pcm.length);
  wav.set(pcm, 44);
  const core = fake({
    "dictation.read": ({ offset, length }) => {
      const start = Number(offset);
      const take = Math.min(Number(length), 16, wav.length - start);
      return {
        base64: btoa(String.fromCharCode(...wav.subarray(start, start + take))),
        total: wav.length,
      };
    },
  });
  const blob = await createNativeRecordings(
    core.request,
    noLegacy,
  ).recordingAudio(row);
  expect(blob.type).toBe("audio/wav");
  expect(new Uint8Array(await blob.arrayBuffer())).toEqual(wav);
  expect(
    core.calls.filter((call) => call.method === "dictation.read").length,
  ).toBeGreaterThan(1);
});

it("claims a transcription through the core and returns null when refused", async () => {
  const core = fake({
    "dictation.begin": (params) =>
      params.chatId === "chat-1" ? "attempt-1" : null,
  });
  const storage = createNativeRecordings(core.request, noLegacy);
  expect(await storage.beginTranscription(row)).toBe("attempt-1");
  expect(
    await storage.beginTranscription({ ...row, chatId: "other" }),
  ).toBeNull();
});

it("trashing clears the attempt in the core, so a late transcript is refused", async () => {
  const core = fake({ "dictation.update": () => null });
  await createNativeRecordings(core.request, noLegacy).trashRecording(row);
  const update = core.calls.find((call) => call.method === "dictation.update");
  expect(update?.params.row).toMatchObject({
    id: "rec-1",
    chatId: "chat-1",
    transcriptionAttempt: null,
    deletedAt: expect.any(Number),
  });
});

it("imports an old recording once, keeps a failed one as an error row, and marks each done", async () => {
  const core = fake({
    "dictation.import": () => ({ done: false }),
    "dictation.chunk": () => null,
    "dictation.delete": () => null,
    "dictation.save": () => null,
    "dictation.imported": () => null,
  });
  const source: LegacyRecordingSource = {
    list: async () => [row, { ...row, id: "bad-1" }],
    chunks: async function* (id) {
      if (id === "bad-1") {
        yield* [];
        throw new Error("Saved audio is damaged.");
      }
      yield { index: 0, pcm: new Int16Array([1, 2]) };
    },
  };
  await importLegacyRecordings(core.request, source);
  const saved = core.calls.find((call) => call.method === "dictation.save");
  expect(saved?.params.row).toMatchObject({
    id: "bad-1",
    samples: 0,
    state: "ready",
    error: expect.stringMatching(/Saved audio is damaged/),
  });
  expect(
    core.calls
      .filter((call) => call.method === "dictation.imported")
      .map((call) => call.params.id),
  ).toEqual(["rec-1", "bad-1"]);
  const done = fake({ "dictation.import": () => ({ done: true }) });
  await importLegacyRecordings(done.request, {
    list: async () => [row],
    chunks: async function* () {
      yield* [];
      throw new Error("must not read a finished import");
    },
  });
  expect(done.calls.map((call) => call.method)).toEqual(["dictation.import"]);
});

it("the read slice size fits the core limit", () => {
  const source = readFileSync(
    new URL(
      "../../../../packages/client-core/src/dictation.rs",
      import.meta.url,
    ),
    "utf8",
  );
  const limit = /const MAX_READ_BYTES: u64 = (\d+) \* 1024;/.exec(source);
  expect(limit).not.toBeNull();
  expect(RECORDING_READ_BYTES).toBeLessThanOrEqual(Number(limit![1]) * 1024);
});
