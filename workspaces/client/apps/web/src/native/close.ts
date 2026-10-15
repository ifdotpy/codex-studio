import { nativeHost } from "./client";
import { isNativeFrame } from "./frame";
import { guardNativeClose, prepareNativeClose } from "./closeState";
import { serverParentOrigin } from "../servers/environment";

const QUERY = "studio-native-close-query";
const REPLY = "studio-native-close-reply";
const RESUME = "studio-native-close-resume";
const releases = new Set<() => void>();

function freeze() {
  const previous = document.documentElement.inert;
  document.documentElement.inert = true;
  return () => {
    document.documentElement.inert = previous;
  };
}

export function bindNativeCloseFrames(frames: Map<string, HTMLIFrameElement>) {
  return guardNativeClose(async () => {
    const original = [...frames];
    await Promise.all(
      original.map(
        ([owner, frame]) =>
          new Promise<void>((resolve, reject) => {
            if (!frame.isConnected || !frame.contentWindow) {
              reject(
                new Error("A server view changed while closing. Try again."),
              );
              return;
            }
            const url = new URL(frame.src);
            const origin = `${url.protocol}//${url.host}`;
            const id = crypto.randomUUID();
            const source = frame.contentWindow;
            const release = () => {
              source.postMessage({ kind: RESUME, id }, origin);
              releases.delete(release);
            };
            releases.add(release);
            const finish = (error?: Error) => {
              clearTimeout(timer);
              window.removeEventListener("message", receive);
              if (error) reject(error);
              else resolve();
            };
            const receive = (event: MessageEvent) => {
              if (
                event.source !== source ||
                event.origin !== origin ||
                event.data?.kind !== REPLY ||
                event.data.id !== id
              )
                return;
              if (
                frames.get(owner) !== frame ||
                !frame.isConnected ||
                event.data.ready !== true
              )
                finish(
                  new Error(
                    event.data.error || "A server view is not ready to close.",
                  ),
                );
              else finish();
            };
            const timer = setTimeout(
              () =>
                finish(
                  new Error(
                    "A server view did not confirm that its changes are saved.",
                  ),
                ),
              10_000,
            );
            window.addEventListener("message", receive);
            source.postMessage({ kind: QUERY, id }, origin);
          }),
      ),
    );
    if (
      frames.size !== original.length ||
      original.some(([owner, frame]) => frames.get(owner) !== frame)
    )
      throw new Error("The server views changed while closing. Try again.");
  });
}

/** Install before React mounts, so every editable native view joins close checks. */
export async function initializeNativeClose() {
  if (isNativeFrame()) {
    let active: { id: string; release: () => void } | undefined;
    window.addEventListener("message", async (event) => {
      if (event.source !== window.parent || event.origin !== serverParentOrigin)
        return;
      const { kind, id } = event.data || {};
      if (kind === RESUME && active && active.id === id) {
        active.release();
        active = undefined;
      }
      if (kind !== QUERY || typeof id !== "string" || active) return;
      active = { id, release: freeze() };
      try {
        await prepareNativeClose();
        window.parent.postMessage(
          { kind: REPLY, id, ready: true },
          serverParentOrigin,
        );
      } catch (error) {
        window.parent.postMessage(
          { kind: REPLY, id, ready: false, error: String(error) },
          serverParentOrigin,
        );
      }
    });
    return;
  }
  const host = nativeHost();
  if (!host?.event) return;
  await host.event.listen("studio-close-request", async ({ payload }) => {
    const id = (payload as { id: number }).id;
    const release = freeze();
    document.getElementById("studio-close-error")?.remove();
    try {
      await prepareNativeClose();
      await host.core.invoke("close_response", { id, ready: true });
    } catch (error) {
      release();
      const pendingReleases = [...releases];
      for (const resume of pendingReleases) resume();
      const message = document.createElement("div");
      message.id = "studio-close-error";
      message.role = "alert";
      message.style.cssText =
        "position:fixed;inset:0 0 auto;z-index:2147483647;padding:16px;background:#7f1d1d;color:white";
      message.textContent = `The window is still open because changes may not be saved. ${String(error)}`;
      document.body.append(message);
      await host.core.invoke("close_response", { id, ready: false });
    }
  });
  await host.core.invoke("close_ready", {});
}
