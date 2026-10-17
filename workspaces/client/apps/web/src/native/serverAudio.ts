import { bindAudioFrames } from "./audioFrames";
import { hasNativeClient, listenNativeEvents, nativeRequest } from "./client";
import { createNativeDictation, dictationBridge } from "./dictation";
import { isNativeFrame } from "./frame";
import { nativeVoice } from "./voice";
import { captureAudio } from "../dictation/capture";

let router: ReturnType<typeof bindAudioFrames> | undefined;
let clients = 0;

/** Native microphone capabilities belong to the connection that acquired them. */
export function createServerAudio() {
  let client:
    | ReturnType<ReturnType<typeof bindAudioFrames>["createClient"]>
    | undefined;
  let closed = false;
  const owned = () => {
    if (closed) throw new Error("The server view closed.");
    if (!client) {
      router ??= bindAudioFrames(nativeRequest, listenNativeEvents);
      client = router.createClient();
      clients++;
    }
    return client;
  };
  const request: typeof nativeRequest = async <T>(
    method: string,
    params: unknown,
    id?: string,
  ): Promise<T> => {
    if (closed) throw new Error("The server view closed.");
    // Legacy embedded views already pass through the shell's capability router.
    if (isNativeFrame()) return nativeRequest(method, params, id);
    return (await owned().request(method, params, id)) as T;
  };
  const listen: typeof listenNativeEvents = async (callback) => {
    if (closed) throw new Error("The server view closed.");
    return isNativeFrame()
      ? listenNativeEvents(callback)
      : owned().listen(callback);
  };
  const dictation = createNativeDictation(request, listen);
  return {
    dictationBridge: () =>
      window.codexDesktop || (hasNativeClient() ? dictation.bridge : undefined),
    captureAudio: (onChunk: (pcm: Int16Array) => void) =>
      captureAudio(onChunk, dictation.captureNativeAudio),
    nativeVoice: (
      session: string,
      onEvent: Parameters<typeof nativeVoice>[1],
    ) => nativeVoice(session, onEvent, request, listen),
    close() {
      if (closed) return;
      closed = true;
      if (client) {
        client.close();
        if (--clients === 0) {
          router?.dispose();
          router = undefined;
        }
      }
    },
  };
}
export const defaultServerAudio = {
  dictationBridge,
  captureAudio,
  nativeVoice,
};
