import type {
  BrowserProfileChunk,
  BrowserProfileRecord,
} from "../web/src/native/importBrowserProfile.ts";
import type { exportBrowserProfile } from "./export-browser-profile.ts";
export type ChunkReply =
  | { value: BrowserProfileChunk; error?: never }
  | { error: string; value?: never };
type ChunkForward = {
  kind: "profile-export-chunk";
  id: number;
  payload: string;
};
type ChunkAcknowledgement = {
  kind: "profile-export-reply";
  id: number;
  result: ChunkReply;
};
declare global {
  interface Window {
    __studioExportReplies: Map<number, (result: ChunkReply) => void>;
    studioExportChunk(payload: string): void;
  }
}

// Both functions are serialized into the isolated profile reader and have no module closures.
export async function loadSourceFrame(origin: string) {
  await new Promise<void>((resolve, reject) => {
    const frame = document.createElement("iframe");
    window.__studioExportReplies = new Map();
    addEventListener("message", (event: MessageEvent<ChunkForward>) => {
      if (
        event.source !== frame.contentWindow ||
        event.origin !== origin ||
        event.data?.kind !== "profile-export-chunk"
      )
        return;
      const { id, payload } = event.data;
      window.__studioExportReplies.set(id, (result) => {
        window.__studioExportReplies.delete(id);
        if (!frame.contentWindow)
          throw new Error("The saved profile frame is unavailable.");
        frame.contentWindow.postMessage(
          { kind: "profile-export-reply", id, result },
          origin,
        );
      });
      window.studioExportChunk(payload);
    });
    const timer = setTimeout(
      () => reject(new Error("Source frame did not load")),
      30000,
    );
    frame.onload = () => {
      clearTimeout(timer);
      resolve();
    };
    frame.src = origin + "/";
    document.body.append(frame);
  });
}

export async function captureSourceProfile(
  parentOrigin: string | null,
  exporter: typeof exportBrowserProfile,
) {
  window.__studioExportReplies = new Map();
  if (parentOrigin)
    addEventListener("message", (event: MessageEvent<ChunkAcknowledgement>) => {
      if (
        event.source === parent &&
        event.origin === parentOrigin &&
        event.data?.kind === "profile-export-reply"
      )
        window.__studioExportReplies.get(event.data.id)?.(event.data.result);
    });
  let next = 0;
  const emit = (records: BrowserProfileRecord[]) =>
    new Promise<BrowserProfileChunk>((resolve, reject) => {
      const id = ++next;
      const timer = setTimeout(() => {
        window.__studioExportReplies.delete(id);
        reject(new Error("Saving a profile chunk timed out"));
      }, 120000);
      window.__studioExportReplies.set(id, (result) => {
        clearTimeout(timer);
        window.__studioExportReplies.delete(id);
        if (result.error) reject(new Error(result.error));
        else if (result.value) resolve(result.value);
      });
      const payload = JSON.stringify({ id, records });
      if (parentOrigin)
        parent.postMessage(
          { kind: "profile-export-chunk", id, payload },
          parentOrigin,
        );
      else window.studioExportChunk(payload);
    });
  return exporter(emit);
}
