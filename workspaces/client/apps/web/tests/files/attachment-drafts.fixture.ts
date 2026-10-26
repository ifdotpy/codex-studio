import type { useAttachmentDrafts } from "../../src/components/useAttachmentDrafts.ts";
/** The renderer under test wrote this local document with useAttachmentDrafts. */
export function readStoredAttachments(
  empty: "{}" | null = "{}",
): ReturnType<typeof useAttachmentDrafts>[0] {
  return JSON.parse(
    (localStorage.getItem("codex-agent-attachments:fixture") || empty)!,
  );
}
declare global {
  interface Window {
    uploadRecoveryBrowserReadStoredAttachments: typeof readStoredAttachments;
  }
}
window.uploadRecoveryBrowserReadStoredAttachments = readStoredAttachments;
