import {
  watchResourceConnection,
  type ResourceConnectionState,
} from "../sync/client";

export function watchVoiceTransport(
  onState: (state: ResourceConnectionState) => void,
  stopVoice: () => void,
  watchConnection: typeof watchResourceConnection = watchResourceConnection,
): () => void {
  return watchConnection((state) => {
    onState(state);
    if (state !== "live") stopVoice();
  });
}
