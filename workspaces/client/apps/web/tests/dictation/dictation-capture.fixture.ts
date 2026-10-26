import type {} from "./dictation-ui.fixture.tsx";
import type { captureAudio as capture } from "../../src/dictation/capture.ts";

export const captureAudio: typeof capture = async (onChunk) => {
  window.emitAudio = () => onChunk(new Int16Array(16000).fill(12));
  return {
    sampleRate: 16000,
    stop: async () => {
      window.captureStopped = (window.captureStopped || 0) + 1;
      if (window.failStop)
        throw new Error("Final chunk not confirmed. Earlier audio saved.");
    },
  };
};
