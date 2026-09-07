import assert from "node:assert/strict";
import vm from "node:vm";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
const ts = createRequire(new URL("../web/package.json", import.meta.url))(
  "typescript",
);
const source = (
  await readFile(
    new URL("../web/src/dictation/capture.ts", import.meta.url),
    "utf8",
  )
).replace(/^import workletURL[^\n]+/, 'const workletURL="fixture.js";');
const code = ts.transpileModule(source, {
  compilerOptions: {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.CommonJS,
  },
}).outputText;
for (const mode of [
  "constructor-failure",
  "worklet-failure",
  "flush-failure",
  "success",
]) {
  let stopped = 0,
    closed = 0;
  class AudioContext {
    constructor() {
      if (mode === "constructor-failure") throw Error("Context unavailable");
      this.sampleRate = 16000;
      this.destination = {};
      this.audioWorklet = {
        addModule: async () => {
          if (mode === "worklet-failure") throw Error("Worklet unavailable");
        },
      };
    }
    createMediaStreamSource() {
      return { connect: (node) => node, disconnect() {} };
    }
    createGain() {
      return { gain: {}, connect() {} };
    }
    async resume() {}
    async close() {
      closed++;
    }
  }
  class AudioWorkletNode {
    constructor() {
      this.port = {
        postMessage: () => {
          if (mode !== "flush-failure")
            queueMicrotask(() => this.port.onmessage({ data: "stopped" }));
        },
      };
    }
    connect(node) {
      return node;
    }
    disconnect() {}
  }
  const context = {
    exports: {},
    navigator: {
      mediaDevices: {
        getUserMedia: async () => ({
          getTracks: () => [
            {
              stop() {
                stopped++;
              },
            },
          ],
        }),
      },
    },
    AudioContext,
    AudioWorkletNode,
    setTimeout: (fn) => {
      setImmediate(fn);
    },
    Promise,
    Error,
    queueMicrotask,
  };
  vm.runInNewContext(code, context);
  if (mode.endsWith("failure") && mode !== "flush-failure")
    await assert.rejects(
      context.exports.captureAudio(() => {}),
      /unavailable/,
    );
  else {
    const capture = await context.exports.captureAudio(() => {});
    if (mode === "flush-failure")
      await assert.rejects(capture.stop(), /final audio chunk/);
    else await capture.stop();
  }
  assert.equal(stopped, 1);
  assert.equal(closed, mode === "constructor-failure" ? 0 : 1);
}
console.log(
  "PASS: actual capture code closes microphone on context/worklet failure; flush timeout reports incomplete tail and still closes stream/context; acknowledged stop succeeds.",
);
