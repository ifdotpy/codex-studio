import type { useSyncedDrafts } from "../../src/sync/drafts.ts";
declare global {
  interface Window {
    corruptDraft_draft: ReturnType<typeof useSyncedDrafts>;
  }
}
import React from "react";
import { createRoot } from "react-dom/client";
export async function mount() {
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.corruptDraft_draft = useSyncedDrafts();
      return null;
    }),
  );
}
