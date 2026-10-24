import { randomUUID } from "../uuid";
import { audioFrameMethod, bindAudioFrames } from "./audioFrames";
import { preferenceScope } from "./preferenceState";

const REQUEST = "studio-native-frame-request";
const RESPONSE = "studio-native-frame-response";
const SHELL_ORIGIN = "tauri://localhost";

export function isNativeFrame() {
  return globalThis.location?.protocol === "studio-view:";
}
// TODO(attar-sync): Replace this postMessage request/reply hop with Attar's
// synchronous, owner-scoped native bridge for local commits. Preserve server
// isolation and the original request ID; do not make network calls synchronous.
export function frameRequest<T>(
  method: string,
  params: unknown,
  id: string = randomUUID(),
): Promise<T> {
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timeout);
      window.removeEventListener("message", receive);
      window.removeEventListener("pagehide", closed);
    };
    const closed = () => {
      cleanup();
      reject(
        new Error(
          "The server view closed. Pending command outcomes may be unknown.",
        ),
      );
    };
    const receive = (event: MessageEvent) => {
      if (
        event.source !== window.parent ||
        event.origin !== SHELL_ORIGIN ||
        event.data?.kind !== RESPONSE ||
        event.data.id !== id
      )
        return;
      cleanup();
      if (event.data.error) reject(new Error(event.data.error));
      else resolve(event.data.result as T);
    };
    const timeout = setTimeout(
      () => {
        cleanup();
        reject(
          new Error(
            "The native service did not respond. The request was not repeated.",
          ),
        );
      },
      method === "speech.transcribe"
        ? 76 * 60_000
        : method === "desktop.requestMicrophone"
          ? 5 * 60_000
          : 30_000,
    );
    window.addEventListener("message", receive);
    window.addEventListener("pagehide", closed, { once: true });
    window.parent.postMessage(
      { kind: REQUEST, id, method, params },
      SHELL_ORIGIN,
    );
  });
}

export function bindNativeFrames(
  frames: Map<string, HTMLIFrameElement>,
  invoke: (method: string, params: unknown, id?: string) => Promise<unknown>,
  listen?: (callback: (value: unknown) => void) => Promise<() => void>,
) {
  const audio = bindAudioFrames(invoke, listen);
  const active = new Map<HTMLIFrameElement, Set<string>>();
  const saves = new Map<string, HTMLIFrameElement>();
  let disposed = false;
  const receive = async (event: MessageEvent) => {
    if (event.data?.kind !== REQUEST) return;
    const entry = [...frames].find(([owner, frame]) => {
      const url = new URL(frame.src);
      return (
        frame.isConnected &&
        frame.contentWindow === event.source &&
        url.protocol === "studio-view:" &&
        `studio-view://${url.host}` === event.origin &&
        url.searchParams.get("studio-server") === owner
      );
    });
    if (!entry) return;
    const [owner, frame] = entry;
    const { id, method, params } = event.data;
    if (typeof id !== "string" || !id || id.length > 128) return;
    const reply = (value: { result: unknown } | { error: string }) => {
      if (!disposed && frame.isConnected && frames.get(owner) === frame)
        frame.contentWindow?.postMessage(
          { kind: RESPONSE, id, ...value },
          event.origin,
        );
    };
    // Remote frames cannot use the local client's data. The native host also
    // checks draft workspace ownership against its verified sync identity.
    const localService =
      owner === "local" &&
      [
        "desktop.fileAction",
        "desktop.revealPath",
        "desktop.showVaultKeys",
        "desktop.pickDirectory",
        "desktop.pickAttachments",
        "identity.get",
        "sync.open",
        "drafts.view",
        "drafts.import",
        "drafts.edit",
        "drafts.dismiss",
        "drafts.refresh",
      ].includes(method);
    const saveMethod = [
      "desktop.saveBegin",
      "desktop.saveChunk",
      "desktop.saveFinish",
      "desktop.saveCancel",
    ].includes(method);
    if (
      saveMethod &&
      method !== "desktop.saveBegin" &&
      saves.get(params?.value?.transfer) !== frame
    )
      return reply({ error: "This save does not belong to this server view." });
    const audioMethod = audioFrameMethod(method, params);
    if (
      !audioMethod &&
      !saveMethod &&
      method !== "markdown.parse" &&
      method !== "desktop.openExternal" &&
      method !== "desktop.openSignIn" &&
      method !== "browser.preferences" &&
      method !== "browser.profile" &&
      method !== "browser.profileChunk" &&
      !localService
    )
      return reply({
        error: "This native service is not available to server views.",
      });
    if (
      method === "desktop.openSignIn" &&
      params?.value?.forward !== undefined &&
      (owner === "local" || params.value.forward?.server !== owner)
    )
      return reply({ error: "This sign-in belongs to another server view." });
    const pending = active.get(frame) ?? new Set<string>();
    if (pending.has(id) || pending.size >= 32)
      return reply({
        error:
          "The server view native queue is full or this request is already pending.",
      });
    active.set(frame, pending);
    pending.add(id);
    try {
      const result = audioMethod
        ? await audio.request(method, params, id, {
            frame,
            origin: event.origin,
            live: () =>
              !disposed && frame.isConnected && frames.get(owner) === frame,
          })
        : method === "browser.profileChunk"
          ? await invoke(
              "profile.chunk",
              { owner: `server:${owner}`, sha256: params?.sha256 },
              id,
            )
          : method === "browser.profile"
            ? await invoke("profile.snapshot", { owner: `server:${owner}` }, id)
            : method === "browser.preferences"
              ? await preferenceScope(invoke, `server:${owner}`)(
                  params?.method,
                  params?.params,
                  id,
                )
              : await invoke(
                  method,
                  method === "desktop.openSignIn"
                    ? { value: params?.value, signInOwner: owner }
                    : params,
                  id,
                );
      if (method === "desktop.saveBegin") {
        const transfer = (result as { transfer?: string })?.transfer;
        if (typeof transfer !== "string")
          throw new Error("Invalid native save reservation.");
        if (disposed || !frame.isConnected || frames.get(owner) !== frame) {
          await invoke("desktop.saveCancel", { value: { transfer } });
        } else saves.set(transfer, frame);
      }
      reply({ result });
    } catch (error) {
      reply({ error: error instanceof Error ? error.message : String(error) });
    } finally {
      if (method === "desktop.saveFinish" || method === "desktop.saveCancel")
        saves.delete(params?.value?.transfer);
      pending.delete(id);
      if (!pending.size) active.delete(frame);
    }
  };
  window.addEventListener("message", receive);
  return () => {
    disposed = true;
    audio.dispose();
    for (const transfer of saves.keys())
      void invoke("desktop.saveCancel", { value: { transfer } }).catch(
        () => {},
      );
    saves.clear();
    window.removeEventListener("message", receive);
  };
}
