import { hasNativeClient, nativeRequest } from "./client";
import { isRemoteServerView } from "../servers/environment";

export function canPickDirectory() {
  return (
    typeof window.codexDesktop?.pickDirectory === "function" ||
    (hasNativeClient() && !isRemoteServerView)
  );
}
export function pickDirectory(): Promise<string | null> {
  if (window.codexDesktop?.pickDirectory)
    return window.codexDesktop.pickDirectory();
  if (!canPickDirectory())
    return Promise.reject(new Error("Native folder selection is unavailable."));
  return nativeRequest("desktop.pickDirectory", {});
}
export function canPickFiles() {
  return (
    typeof window.codexDesktop?.pickFiles === "function" ||
    (hasNativeClient() && !isRemoteServerView)
  );
}
export async function pickFiles(): Promise<File[]> {
  if (!canPickFiles()) throw new Error("Native file selection is unavailable.");
  const files = window.codexDesktop?.pickFiles
    ? (await window.codexDesktop.pickFiles()).map((file) => ({
        ...file,
        base64: file.data,
      }))
    : await nativeRequest<{ name: string; mime: string; base64: string }[]>(
        "desktop.pickAttachments",
        {},
      );
  return files.map(
    (file) =>
      new File(
        [Uint8Array.from(atob(file.base64), (c) => c.charCodeAt(0))],
        file.name,
        { type: file.mime },
      ),
  );
}

export function canRevealPath() {
  return (
    !isRemoteServerView &&
    (!!window.codexDesktop?.revealPath || hasNativeClient())
  );
}
export async function revealPath(path: string): Promise<void> {
  if (!canRevealPath())
    throw new Error("Local folder reveal is unavailable in this server view.");
  if (window.codexDesktop?.revealPath)
    return window.codexDesktop.revealPath(path);
  await nativeRequest("desktop.revealPath", { value: path });
}

export function canShowVaultKeys() {
  return (
    !isRemoteServerView &&
    (!!window.codexDesktop?.showVaultKeys || hasNativeClient())
  );
}
export async function showVaultKeys(prefix: string): Promise<void> {
  if (!canShowVaultKeys())
    throw new Error("Vault keys are unavailable in this server view.");
  if (window.codexDesktop?.showVaultKeys)
    return window.codexDesktop.showVaultKeys(prefix);
  await nativeRequest("desktop.showVaultKeys", { value: prefix });
}
