import type { BrowserDownload } from "./downloads";

export function saveBrowserDownload(
  result: BrowserDownload,
  name = result.name,
): boolean {
  const url = URL.createObjectURL(result.blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
  return true;
}
export function saveBrowserText(name: string, text: string): boolean {
  return saveBrowserDownload({
    blob: new Blob([text], { type: "text/plain" }),
    name,
    truncated: false,
  });
}
