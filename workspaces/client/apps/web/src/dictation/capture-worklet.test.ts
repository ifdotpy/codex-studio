import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { test } from "vitest";

test("AudioWorklet converts and flushes PCM samples", async () => {
  let Processor;
  const messages = [];
  vm.runInNewContext(
    await readFile(new URL("./capture-worklet.js", import.meta.url), "utf8"),
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
});
