import { hasNativeClient, nativeRequest } from "./native/client";
import type { ApiDownloadResult, NativeDownloadResult } from "./api";

export type NativeDownload = NativeDownloadResult;
export type BrowserDownload = Extract<ApiDownloadResult, { blob: Blob }>;

export async function saveApiDownload(
  result: NativeDownload | BrowserDownload,
  name?: string,
): Promise<boolean> {
  // The native action consumed the gesture before HTTP and already showed Save.
  if ("nativeHandle" in result) return result.saved;
  return (await import("./browserDownloads")).saveBrowserDownload(result, name);
}
export async function saveTextDownload(
  name: string,
  text: string,
): Promise<boolean> {
  if (hasNativeClient())
    return nativeRequest<boolean>("desktop.saveText", {
      value: { name, text },
    });
  return (await import("./browserDownloads")).saveBrowserText(name, text);
}
