import { serverViewId } from "./environment";
import type { DesktopBridge } from "../desktop";
export type ServerNativeMethod =
  | "requestMicrophone"
  | "prepareTranscription"
  | "transcribeAudio"
  | "cancelTranscription"
  | "pickDirectory"
  | "pickFiles"
  | "revealPath"
  | "saveFile"
  | "fileAction"
  | "openExternal"
  | "getBackendUpdate";
export function installFrameDesktopBridge() {
  if (!serverViewId || window.parent === window || window.codexDesktop) return;
  // Only our same-origin shell can supply the main-frame native broker.
  let desktop: DesktopBridge | undefined;
  try {
    desktop = window.parent.codexDesktop;
  } catch {
    return;
  }
  if (!desktop?.serverNativeAction) return;
  const call = (method: ServerNativeMethod, value?: unknown) =>
    desktop!.serverNativeAction!(serverViewId!, method, value);
  const bridge = {
    platform: desktop.platform,
    requestMicrophone: () => call("requestMicrophone"),
    prepareTranscription: () => call("prepareTranscription"),
    transcribeAudio: (value: unknown) => call("transcribeAudio", value),
    cancelTranscription: (value: string) => call("cancelTranscription", value),
    onTranscriptionProgress: desktop.onTranscriptionProgress,
    pickDirectory: () => call("pickDirectory"),
    pickFiles: () => call("pickFiles"),
    revealPath: (value: string) => call("revealPath", value),
    saveFile: (value: unknown) => call("saveFile", value),
    ...(serverViewId === "local"
      ? { fileAction: (value: unknown) => call("fileAction", value) }
      : {}),
    openExternal: (value: string) => call("openExternal", value),
    getBackendUpdate: () => call("getBackendUpdate"),
    notify: () => Promise.resolve(false),
    onNavigate: () => () => {},
  } as DesktopBridge;
  window.codexDesktop = Object.freeze(bridge);
}
