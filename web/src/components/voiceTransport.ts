import {
  watchResourceConnection,
  type ResourceConnectionState,
} from "../sync/client";

export function watchVoiceTransport(
  onState: (state: ResourceConnectionState) => void,
  stopVoice: () => void,
): () => void {
  return watchResourceConnection((state) => {
    onState(state);
    if (state !== "live") stopVoice();
  });
}
