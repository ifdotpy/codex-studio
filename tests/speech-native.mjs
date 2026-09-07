import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFile, mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { wav } from "../web/src/dictation/storage.ts";
const { validateAudio, transcribe } = createRequire(import.meta.url)(
  "../desktop/speech.cjs",
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
const vm = await import("node:vm");
let Processor;
const messages = [];
vm.runInNewContext(
  await readFile(
    new URL("../web/src/dictation/capture-worklet.js", import.meta.url),
    "utf8",
  ),
  {
    AudioWorkletProcessor: class {
      constructor() {
        this.port = { postMessage: (value) => messages.push(value) };
      }
    },
    registerProcessor: (_name, value) => {
      Processor = value;
    },
    Int16Array,
    Math,
  },
);
const processor = new Processor();
processor.process([[new Float32Array([0, 1, -1, 2])]]);
processor.port.onmessage({ data: "stop" });
assert.deepEqual([...messages[0]], [0, 32767, -32767, 32767]);
assert.equal(messages[1], "stopped");
assert.equal(processor.process([]), false);
console.log(
  "PASS: production AudioWorklet sample conversion, clipping, final chunk flush and stop acknowledgement.",
);
