import "./attachment-drafts.fixture.ts";
type AttachmentState = ReturnType<typeof useAttachmentDrafts>;
type FormState = ReturnType<typeof useFormDraft>;
export type RecoveryHarness = {
  attachments: AttachmentState[0];
  setAttachments: AttachmentState[1];
  scope: string;
  setScope: React.Dispatch<React.SetStateAction<string>>;
  recovery: ReturnType<typeof useUploadRecovery>;
  profile: FormState[0];
  setProfile: FormState[1];
  rule: FormState[0];
  setRule: FormState[1];
  pending: () => ReturnType<typeof pendingUploads>;
};
declare global {
  interface Window {
    uploadRecoveryBrowserFixture: RecoveryHarness;
    scope: string;
    notices: string[];
    restoreTransactions: () => void;
    cancelError: string;
    releaseFileRead: () => void;
    uploadRecoveryBrowserMount: typeof mount;
    uploadRecoveryBrowserMountWriter: typeof mountWriter;
  }
}
window.uploadRecoveryBrowserMount = mount;
window.uploadRecoveryBrowserMountWriter = mountWriter;

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import ComposerAttachments from "../../src/components/ComposerAttachments.tsx";
import { useAttachmentDrafts } from "../../src/components/useAttachmentDrafts.ts";
import { useUploadRecovery } from "../../src/components/useUploadRecovery.ts";
import { useFormDraft } from "../../src/components/useFormDraft.ts";
import { queueUploads, pendingUploads } from "../../src/sync/uploads.ts";
export function mount() {
  createRoot(document.body).render(
    React.createElement(function Harness() {
      window.notices ||= [];
      const notify = (message: string) => window.notices.push(message);
      const [scope, setScope] = React.useState(window.scope);
      const [attachments, setAttachments] = useAttachmentDrafts(
        "codex-agent-attachments:fixture",
        notify,
      );
      const recovery = useUploadRecovery("fixture", scope, setAttachments);
      const [profile, setProfile] = useFormDraft(
        "fixture-profile:" + scope,
        notify,
      );
      const [rule, setRule] = useFormDraft("fixture-rule:" + scope, notify);
      window.uploadRecoveryBrowserFixture = {
        attachments,
        setAttachments,
        scope,
        setScope,
        recovery,
        profile,
        setProfile,
        rule,
        setRule,
        pending: () => pendingUploads("fixture"),
      };
      return React.createElement(
        MantineProvider,
        null,
        React.createElement(ComposerAttachments, {
          notify,
          assets: attachments.lead || [],
          disabled: false,
          uploading: false,
          add: (files) =>
            queueUploads(
              "fixture",
              scope === "b".repeat(32) ? "other" : "lead",
              files,
              scope,
            ).then(() => {}),
          remove: () => {},
        }),
      );
    }),
  );
}
export function mountWriter() {
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
