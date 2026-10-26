import type {} from "./upload-recovery-browser.fixture.tsx";
declare global {
  interface Window {
    uploadRecoveryBrowserRaceMount: typeof mount;
  }
}
window.uploadRecoveryBrowserRaceMount = mount;

import React from "react";
import { createRoot } from "react-dom/client";
import { useAttachmentDrafts } from "../../src/components/useAttachmentDrafts.ts";
export function mount() {
  createRoot(document.body).render(
    React.createElement(function Writer() {
      const [attachments, setAttachments] = useAttachmentDrafts(
        "codex-agent-attachments:fixture",
        () => {},
      );
      window.uploadRecoveryBrowserWriter = { attachments, setAttachments };
      return null;
    }),
  );
}
