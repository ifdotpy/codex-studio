import type {} from "./dictation-ui.fixture.tsx";
import type { captureAudio as capture } from "../../src/dictation/capture.ts";

export const captureAudio: typeof capture = async (onChunk) => {
  window.dictationFixture.emitAudio = () =>
    onChunk(new Int16Array(16000).fill(12));
  return {
    sampleRate: 16000,
    stop: async () => {
      window.dictationFixture.captureStopped++;
      if (window.dictationFixture.failStop)
        throw new Error("Final chunk not confirmed. Earlier audio saved.");
    },
  };
};
