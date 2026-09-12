import type { DesktopBridge } from "./desktop";

export interface FileTarget {
  agent?: string;
  path?: string;
  asset?: string;
}
export interface SaveFile {
  name: string;
  mime?: string;
  data: ArrayBuffer | Uint8Array | string;
}

export function canUseNativeFiles(): boolean {
  return typeof window.codexDesktop?.fileAction === "function";
}

function fileAction(action: "open" | "reveal" | "preview", target: FileTarget) {
  const native = window.codexDesktop?.fileAction;
  if (!native) return Promise.resolve(false);
  // Omit view-only fields such as line numbers from the native identity.
  const identity = Object.fromEntries(
    ["agent", "path", "asset"].flatMap((key) => {
      const value = target[key as keyof FileTarget];
      return value === undefined ? [] : [[key, value]];
    }),
  );
  return native({ action, target: identity });
}

export const openFile = (target: FileTarget) => fileAction("open", target);
export const revealFile = (target: FileTarget) => fileAction("reveal", target);
export const previewFile = (target: FileTarget) => fileAction("preview", target);

export async function saveFile(value: SaveFile): Promise<boolean> {
  const data = typeof value.data === "string"
    ? new TextEncoder().encode(value.data).buffer
    : value.data instanceof Uint8Array
      ? new Uint8Array(value.data).buffer
      : value.data;
  if (!(data instanceof ArrayBuffer) || data.byteLength > 64 * 1024 * 1024)
    throw new Error("Save accepts at most 64 MiB of file data.");
  const name = value.name.replaceAll("\\", "/").split("/").pop();
  if (!name || name === "." || name === ".." || /[\x00-\x1f]/.test(name))
    throw new Error("Invalid file name.");
  const native: DesktopBridge["saveFile"] = window.codexDesktop?.saveFile;
  // Call the bridge before an await, while the initiating gesture is available.
  if (native) return native({ name, data });
  const file = new File([data], name, { type: value.mime || "application/octet-stream" });
  if (navigator.canShare?.({ files: [file] }) && navigator.share) {
    try {
      await navigator.share({ files: [file] });
      return true;
    } catch (error) {
      if (error instanceof Error && error.name === "AbortError") return false;
      throw error;
    }
  }
  const url = URL.createObjectURL(file);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
  // Browser fallback starts a save; it cannot report the user's final choice.
  return true;
}
