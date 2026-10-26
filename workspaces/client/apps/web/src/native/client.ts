import { frameRequest, isNativeFrame } from "./frame";
import {
  createNativeClient,
  invokeClientRequest,
  type LegacyNativeHost,
} from "./clientCore";
export type * from "./clientCore";

export function nativeHost(): LegacyNativeHost | undefined {
  return (globalThis as typeof globalThis & { __TAURI__?: LegacyNativeHost })
    .__TAURI__;
}
const client = createNativeClient({
  available: () =>
    typeof nativeHost()?.core?.invoke === "function" || isNativeFrame(),
  async send<T>(method: string, params: unknown, id: string): Promise<T> {
    if (isNativeFrame()) return frameRequest<T>(method, params, id);
    const native = nativeHost();
    if (!native) throw new Error("The native client is unavailable.");
    return invokeClientRequest<T>(
      (command, args) => native.core.invoke(command, args),
      method,
      params,
      id,
    );
  },
  async listen(callback) {
    if (isNativeFrame()) {
      const receive = (event: MessageEvent) => {
        if (
          event.source === window.parent &&
          event.origin === "tauri://localhost" &&
          (event.data?.kind === "studio-native-audio-event" ||
            event.data?.kind === "studio-native-service-event")
        )
          callback(event.data);
      };
      window.addEventListener("message", receive);
      return () => window.removeEventListener("message", receive);
    }
    const native = nativeHost();
    if (!native?.event) throw new Error("Native events are unavailable.");
    return native.event.listen("studio-client-event", (event) =>
      callback(event.payload),
    );
  },
});
export const {
  hasNativeClient,
  nativeRequest,
  listenNativeEvents,
  nativeIdentity,
  prepareLegacyNativePreferences,
  nativeStorageCutover,
  nativeServerContext,
} = client;
