import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import type { StudioCapture } from "./capture-worklet.ts";
import { test } from "vitest";

test("AudioWorklet converts and flushes PCM samples", async () => {
  let Processor: (new () => StudioCapture) | undefined;
  const messages: (Int16Array | string)[] = [];
  vm.runInNewContext(
    stripTypeScriptTypes(
      (
        await readFile(new URL("./capture-worklet.ts", import.meta.url), "utf8")
      ).replace("export class", "class"),
    ),
    {
      AudioWorkletProcessor: class {
        port = {
          postMessage: (value: Int16Array | string) => messages.push(value),
        };
      },
      registerProcessor: (_name: string, value: new () => StudioCapture) => {
        Processor = value;
      },
      Int16Array,
      Math,
    },
  );
  assert.ok(Processor);
  const processor = new Processor();
  processor.process([[new Float32Array([0, 1, -1, 2])]]);
  processor.port.onmessage?.call(
    processor.port,
    new MessageEvent("message", { data: "stop" }),
  );
  assert.ok(messages[0] instanceof Int16Array);
  assert.deepEqual([...messages[0]], [0, 32767, -32767, 32767]);
  assert.equal(messages[1], "stopped");
  assert.equal(processor.process([]), false);
  console.log(
    "PASS: production AudioWorklet sample conversion, clipping, final chunk flush and stop acknowledgement.",
  );
});
