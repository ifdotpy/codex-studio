import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import type { StudioCapture } from "./capture-worklet.ts";
import { test } from "vitest";

// The VM fixture only uses the message data; the rest of the DOM event is absent.
type CaptureFixture = Pick<StudioCapture, "process"> & {
  port: Pick<StudioCapture["port"], "postMessage"> & {
    onmessage(event: Pick<MessageEvent, "data">): void;
  };
};

test("AudioWorklet converts and flushes PCM samples", async () => {
  let Processor: (new () => CaptureFixture) | undefined;
  const messages: (Int16Array | string)[] = [];
  vm.runInNewContext(
    stripTypeScriptTypes(
      (
        await readFile(new URL("./capture-worklet.ts", import.meta.url), "utf8")
      ).replace("export class", "class"),
    ),
    {
      AudioWorkletProcessor: class {
        declare port: Pick<MessagePort, "postMessage">;
        constructor() {
          this.port = {
            postMessage: (value: Int16Array | string) => messages.push(value),
          };
        }
      },
      registerProcessor: (_name: string, value: new () => CaptureFixture) => {
        Processor = value;
      },
      Int16Array,
      Math,
    },
  );

  const processor = new Processor!();
  processor.process([[new Float32Array([0, 1, -1, 2])]]);
  processor.port.onmessage({ data: "stop" });
  assert.deepEqual([...(messages[0] as Int16Array)], [0, 32767, -32767, 32767]);
  assert.equal(messages[1], "stopped");
  assert.equal(processor.process([]), false);
  console.log(
    "PASS: production AudioWorklet sample conversion, clipping, final chunk flush and stop acknowledgement.",
  );
});
