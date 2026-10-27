import type { ConnectionHealth } from "../sync/connectionState";
import { watchResourceConnection } from "../sync/client";

export function watchVoiceTransport(
  onState: (state: ConnectionHealth) => void,
  stopVoice: () => void,
  watchConnection: typeof watchResourceConnection = watchResourceConnection,
): () => void {
  return watchConnection((state) => {
    onState(state);
    if (state !== "live") stopVoice();
  });
}
