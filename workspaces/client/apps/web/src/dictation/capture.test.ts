import assert from "node:assert/strict";
import { afterEach, it, vi } from "vitest";
import { captureAudio } from "./capture";

vi.mock("../native/client", () => ({ hasNativeClient: () => false }));
vi.mock("../native/dictation", () => ({ captureNativeAudio: vi.fn() }));
afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

it("cleans up audio resources after construction, worklet and flush failures", async () => {
  vi.useFakeTimers();
  for (const mode of [
    "constructor-failure",
    "worklet-failure",
    "flush-failure",
    "success",
  ]) {
    let stopped = 0,
      closed = 0;
    class AudioContext {
      sampleRate: number;
      destination: Record<string, never>;
      audioWorklet: Pick<globalThis.AudioWorklet, "addModule">;
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
        return { connect: (node: AudioWorkletNode) => node, disconnect() {} };
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
      port = {
        onmessage: (_event: Pick<MessageEvent, "data">) => {},
        postMessage: () => {
          if (mode !== "flush-failure")
            queueMicrotask(() => this.port.onmessage({ data: "stopped" }));
        },
      };
      connect(node: unknown) {
        return node;
      }
      disconnect() {}
    }
    vi.stubGlobal("navigator", {
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
    });
    vi.stubGlobal("AudioContext", AudioContext);
    vi.stubGlobal("AudioWorkletNode", AudioWorkletNode);
    if (mode.endsWith("failure") && mode !== "flush-failure")
      await assert.rejects(
        captureAudio(() => {}),
        /unavailable/,
      );
    else {
      const capture = await captureAudio(() => {});
      const stopped = capture.stop();
      if (mode === "flush-failure") {
        const rejected = assert.rejects(stopped, /final audio chunk/);
        await vi.advanceTimersByTimeAsync(500);
        await rejected;
      } else await stopped;
    }
    assert.equal(stopped, 1);
    assert.equal(closed, mode === "constructor-failure" ? 0 : 1);
  }
});
