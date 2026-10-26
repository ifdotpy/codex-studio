import { nativeRequest } from "./client";
import type { ApiDownloadResult } from "../api";

// Native build alias for ../downloads. No browser byte owner enters this graph.
export async function saveApiDownload(
  result: ApiDownloadResult,
  _name?: string,
): Promise<boolean> {
  if (
    !("nativeHandle" in result) ||
    !result.nativeHandle ||
    typeof result.saved !== "boolean"
  )
    throw new Error("The native download receipt is invalid.");
  return result.saved;
}
export function saveTextDownload(name: string, text: string): Promise<boolean> {
  return nativeRequest<boolean>("desktop.saveText", { value: { name, text } });
}
