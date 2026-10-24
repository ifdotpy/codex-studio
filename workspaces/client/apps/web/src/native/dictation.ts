import { randomUUID } from "../uuid";
import type { DesktopBridge } from "../desktop";
import { hasNativeClient, listenNativeEvents, nativeRequest } from "./client";
import { isNativeFrame } from "./frame";

type SpeechBridge = Pick<
  DesktopBridge,
  | "requestMicrophone"
  | "prepareTranscription"
  | "transcribeAudio"
  | "cancelTranscription"
  | "onTranscriptionProgress"
>;
function encoded(bytes: Uint8Array) {
  const parts: string[] = [];
  for (let offset = 0; offset < bytes.length; offset += 16384)
    parts.push(String.fromCharCode(...bytes.subarray(offset, offset + 16384)));
  return btoa(parts.join(""));
}
export function createNativeDictation(
  request: typeof nativeRequest = nativeRequest,
  listen: typeof listenNativeEvents = listenNativeEvents,
) {
  const cancelled = new Set<string>();
  const running = new Set<string>();
  const unsubscribe = (subscription: string) =>
    request(isNativeFrame() ? "audio.unsubscribe" : "view.unsubscribe", {
      subscription,
    });
  const bridge: SpeechBridge = {
    requestMicrophone: () => request("desktop.requestMicrophone", {}),
    prepareTranscription: () => request("desktop.prepareTranscription", {}),
    async transcribeAudio(value) {
      if (value.audio.byteLength > 64 * 1024 * 1024)
        throw new Error("Transcription accepts at most 64 MiB of audio.");
      let transfer: string | undefined;
      const check = () => {
        if (cancelled.has(value.id))
          throw new Error("Transcription cancelled. Your recording is saved.");
      };
      try {
        check();
        const reservation = await request<{ transfer: string }>(
          "speech.uploadBegin",
          { permit: value.permit, size: value.audio.byteLength },
        );
        transfer = reservation.transfer;
        const bytes = new Uint8Array(value.audio);
        for (let offset = 0; offset < bytes.length; offset += 256 * 1024) {
          check();
          await request("speech.uploadChunk", {
            transfer,
            offset,
            base64: encoded(bytes.subarray(offset, offset + 256 * 1024)),
          });
        }
        check();
        running.add(value.id);
        return await request("speech.transcribe", {
          transfer,
          id: value.id,
          permit: value.permit,
          locale: value.locale,
        });
      } finally {
        running.delete(value.id);
        cancelled.delete(value.id);
        if (transfer)
          await request("speech.uploadCancel", { transfer }).catch(() => {});
      }
    },
    cancelTranscription(id) {
      cancelled.add(id);
      if (cancelled.size > 128)
        cancelled.delete(cancelled.values().next().value!);
      // The component may cancel while WAV chunks are still being uploaded.
      return running.has(id)
        ? request("desktop.cancelTranscription", { value: id })
        : Promise.resolve(false);
    },
    onTranscriptionProgress(callback) {
      let disposed = false;
      let stop: (() => void) | undefined;
      const subscription = randomUUID();
      void (async () => {
        stop = await listen((event: any) => {
          if (!disposed && event?.event === subscription) callback(event.value);
        });
        if (disposed) {
          stop();
          return;
        }
        await request("view.subscribe", {
          topic: "transcription",
          subscription,
        });
        if (disposed) await unsubscribe(subscription);
      })().catch((error) => {
        stop?.();
        if (!disposed)
          window.dispatchEvent(
            new CustomEvent("desktop-error", { detail: String(error) }),
          );
      });
      return () => {
        disposed = true;
        stop?.();
        void unsubscribe(subscription).catch(() => {});
      };
    },
  };
  async function captureNativeAudio(onChunk: (pcm: Int16Array) => void) {
    const capture = randomUUID(),
      subscription = randomUUID();
    let finish: (() => void) | undefined;
    const flushed = new Promise<void>((resolve) => {
      finish = resolve;
    });
    let chunkError: unknown;
    const stopListening = await listen((event: any) => {
      if (event?.event !== subscription) return;
      if (event.value?.stopped) {
        finish?.();
        return;
      }
      if (typeof event.value?.pcm === "string") {
        try {
          const raw = atob(event.value.pcm);
          if (raw.length % 2) throw new Error("Invalid microphone PCM chunk.");
          const pcm = new Int16Array(raw.length / 2);
          for (let i = 0; i < pcm.length; i++)
            pcm[i] = raw.charCodeAt(i * 2) | (raw.charCodeAt(i * 2 + 1) << 8);
          onChunk(pcm);
        } catch (error) {
          chunkError = error;
        }
      }
    });
    const cleanup = async () => {
      stopListening();
      await unsubscribe(subscription).catch(() => {});
    };
    try {
      await request("view.subscribe", {
        topic: "microphone",
        subscription,
        capture,
      });
      const result = await request<{ sampleRate: number }>(
        "media.captureStart",
        { capture },
      );
      let stopped: Promise<void> | undefined;
      return {
        sampleRate: result.sampleRate,
        stop: () =>
          (stopped ??= (async () => {
            let timer: ReturnType<typeof setTimeout> | undefined;
            try {
              await request("media.captureStop", { capture });
              await Promise.race([
                flushed,
                new Promise<never>((_resolve, reject) => {
                  timer = setTimeout(
                    () =>
                      reject(
                        new Error(
                          "The final microphone chunk was not confirmed. Earlier audio remains saved.",
                        ),
                      ),
                    2000,
                  );
                }),
              ]);
              if (chunkError) throw chunkError;
            } finally {
              clearTimeout(timer);
              await cleanup();
            }
          })()),
      };
    } catch (error) {
      await request("media.captureStop", { capture }).catch(() => {});
      await cleanup();
      throw error;
    }
  }

  return { bridge, captureNativeAudio };
}
const defaults = createNativeDictation();
export const captureNativeAudio = defaults.captureNativeAudio;
export function dictationBridge(): SpeechBridge | undefined {
  return (
    window.codexDesktop || (hasNativeClient() ? defaults.bridge : undefined)
  );
}
