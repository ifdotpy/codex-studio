import "./attachment-drafts.fixture.ts";
import type { RecoveryHarness } from "./upload-recovery-browser.fixture.tsx";
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
      // Base writer exposes only attachment state, omitting recovery/form members.
      window.uploadRecoveryBrowserFixture = {
        attachments,
        setAttachments,
      } as RecoveryHarness;
      return null;
    }),
  );
}
