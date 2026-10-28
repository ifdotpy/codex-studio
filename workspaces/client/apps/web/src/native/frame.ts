import { randomUUID } from "../uuid";
import { audioFrameMethod, bindAudioFrames } from "./audioFrames";
import { preferenceScope } from "./preferenceState";
import { nativeSignInMethods } from "./signInProtocol";
import { coreForServer, type CoreClient } from "./client";

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

// Commands that the core of a frame's own server runs: its data, queue,
// drafts, files and views. Device commands keep their own rules below.
const SERVER_CORE_PREFIXES = [
  "outbox.",
  "drafts.",
  "uploads.",
  "files.",
  "dictation.",
  "projection.",
  "usage.",
  "terminal.",
];
const SERVER_CORE_METHODS = ["api.request", "sync.open", "identity.get"];
const SERVER_VIEW_METHODS = [
  "view.subscribe",
  "view.next",
  "view.resume",
  "view.restart",
  "view.unsubscribe",
];
// View topics of server data. Microphone, voice, transcription and navigation belong to the device.
const SERVER_VIEW_TOPICS = [
  "projection",
  "outbox",
  "drafts",
  "sse",
  "terminal",
];

/** Server management belongs to the workspace shell, never to a server view. */
export function isServerManagementRequest(method: string, url: URL) {
  const path = decodeURIComponent(url.pathname).replace(/\/+$/, "");
  return (
    path.startsWith("/api/multi-server/v1/") ||
    (method === "POST" &&
      (path === "/api/multi-server" || path.startsWith("/api/multi-server/")))
  );
}

function frameManagementRequest(method: string, params: unknown) {
  if (method !== "api.request") return false;
  const value = params as { path?: unknown; method?: unknown } | null;
  if (typeof value?.path !== "string") return true;
  try {
    return isServerManagementRequest(
      typeof value.method === "string"
        ? value.method
        : "body" in (value as object)
          ? "POST"
          : "GET",
      new URL(value.path, "http://localhost"),
    );
  } catch {
    return true;
  }
}

export function serverCoreMethod(method: string, params: unknown) {
  if (method === "dictation.wav") return false;
  if (method === "view.subscribe")
    return SERVER_VIEW_TOPICS.includes(
      String((params as { topic?: unknown } | null)?.topic),
    );
  return (
    SERVER_VIEW_METHODS.includes(method) ||
    SERVER_CORE_METHODS.includes(method) ||
    SERVER_CORE_PREFIXES.some((prefix) => method.startsWith(prefix))
  );
}

export function bindNativeFrames(
  frames: Map<string, HTMLIFrameElement>,
  invoke: (method: string, params: unknown, id?: string) => Promise<unknown>,
  listen?: (callback: (value: unknown) => void) => Promise<() => void>,
  coreFor: (owner: string) => CoreClient = coreForServer,
) {
  const audio = bindAudioFrames(invoke, listen);
  const active = new Map<HTMLIFrameElement, Set<string>>();
  const saves = new Map<string, HTMLIFrameElement>();
  // One listener per server core forwards its events to the frame of that server only.
  const coreEvents = new Map<string, Promise<() => void>>();
  let disposed = false;
  const forwardCoreEvents = (owner: string) => {
    if (coreEvents.has(owner)) return coreEvents.get(owner);
    const listening = coreFor(owner).listen((value) => {
      const frame = frames.get(owner);
      if (disposed || !frame?.isConnected) return;
      frame.contentWindow?.postMessage(
        { kind: "studio-native-core-event", value },
        `studio-view://${new URL(frame.src).host}`,
      );
    });
    coreEvents.set(owner, listening);
    listening.catch(() => coreEvents.delete(owner));
    return listening;
  };
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
    // Local file actions belong to the local server only.
    const localService =
      owner === "local" &&
      [
        "desktop.fileAction",
        "desktop.revealPath",
        "desktop.showVaultKeys",
        "desktop.pickDirectory",
        "desktop.pickAttachments",
      ].includes(method);
    // Server data goes to the core of the frame's own server, never another
    // one. The native host also checks draft workspace ownership.
    const coreMethod = serverCoreMethod(method, params);
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
    const signInMethod = Object.values(nativeSignInMethods).includes(method);
    if (
      !audioMethod &&
      !saveMethod &&
      method !== "markdown.parse" &&
      method !== "desktop.openExternal" &&
      !signInMethod &&
      method !== "browser.preferences" &&
      method !== "browser.profile" &&
      method !== "browser.profileChunk" &&
      !localService &&
      !coreMethod
    )
      return reply({
        error: "This native service is not available to server views.",
      });
    if (coreMethod && frameManagementRequest(method, params))
      return reply({ error: "Use the workspace shell for server management." });
    if (
      method === nativeSignInMethods.open &&
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
      if (coreMethod && method === "view.subscribe")
        await forwardCoreEvents(owner);
      const result = coreMethod
        ? await coreFor(owner).request(method, params, id)
        : audioMethod
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
              ? await invoke(
                  "profile.snapshot",
                  { owner: `server:${owner}` },
                  id,
                )
              : method === "browser.preferences"
                ? await preferenceScope(invoke, `server:${owner}`)(
                    params?.method,
                    params?.params,
                    id,
                  )
                : await invoke(
                    method,
                    signInMethod
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
    for (const listening of coreEvents.values())
      void listening.then((unlisten) => unlisten()).catch(() => {});
    coreEvents.clear();
    for (const transfer of saves.keys())
      void invoke("desktop.saveCancel", { value: { transfer } }).catch(
        () => {},
      );
    saves.clear();
    window.removeEventListener("message", receive);
  };
}
