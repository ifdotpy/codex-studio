import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { wav } from "./storage.ts";
import { test } from "vitest";

test("speech WAV encoding and helper errors remain bounded and retryable", async () => {
  const { validateAudio, transcribe } = createRequire(import.meta.url)(
    "../../../desktop/speech.cjs",
  );
  const data = await wav(
    [new Int16Array([0, 32767, -32768])],
    16000,
  ).arrayBuffer();
  assert.equal(validateAudio(data).length, 50);
  assert.throws(() => validateAudio(new ArrayBuffer(2)), /WAV/);
  const malformed = data.slice(0);
  new DataView(malformed).setUint32(40, 1, true);
  assert.throws(() => validateAudio(malformed), /WAV/);
  await assert.rejects(
    transcribe({ audio: data, locale: "../evil" }, "/missing"),
    /language/,
  );
  await assert.rejects(
    transcribe({ audio: data, locale: "en-US" }, "/missing"),
    /missing/,
  );
  const folder = await mkdtemp(join(tmpdir(), "studio-speech-contract-"));
  try {
    const helper = join(folder, "helper");
    await writeFile(
      helper,
      '#!/bin/sh\nprintf \'{"error":"Permission denied, saved audio retained"}\\n\'\nexit 1\n',
      { mode: 0o700 },
    );
    await assert.rejects(
      transcribe({ audio: data, locale: "en-US" }, helper),
      /Permission denied/,
    );
    await writeFile(
      helper,
      '#!/bin/sh\nprintf \'{"text":"Recovered result","provider":"fixture","onDevice":true}\\n\'\n',
      { mode: 0o700 },
    );
    assert.equal(
      (await transcribe({ audio: data, locale: "ru-RU" }, helper)).text,
      "Recovered result",
    );
  } finally {
    await rm(folder, { recursive: true, force: true });
  }
  console.log(
    "PASS: real WAV encoder/native validation, unsupported data/language, missing helper, failed transcription and retry. No speech permission requested.",
  );
});
