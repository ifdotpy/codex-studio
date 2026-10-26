import type { NativeServerContext } from "../client";
import { createNativeBytes } from "../bytes";
import { createNativeVoiceSession } from "../voiceSession";

/** Native media ports retain handles. No PCM, WAV, File or peer owner enters JS. */
export function createNativeMediaServices(context: NativeServerContext) {
  const bytes = createNativeBytes(context);
  return {
    recordings: {
      listRecordings: bytes.listRecordings,
      deletedRecordings: bytes.deletedRecordings,
      getRecording: bytes.getRecording,
      updateRecording: bytes.updateRecording,
      trashRecording: bytes.trashRecording,
      restoreRecording: bytes.restoreRecording,
      playRecording: bytes.playRecording,
      stopPlayback: bytes.stopPlayback,
      saveRecording: bytes.saveRecording,
    },
    audio: {
      recording: bytes,
      voice: (agent: string) => createNativeVoiceSession(context, agent),
    },
  };
}
