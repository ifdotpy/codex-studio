import { readDraft, readLocalDraft } from "./draft-fixture-helpers.ts";
import type { useSyncedDrafts } from "../../src/sync/drafts.ts";
import type { syncDatabase } from "../../src/sync/client.ts";
declare global {
  interface Window {
    coalescingDraft_readDraft: typeof readDraft;
    coalescingDraft_readLocalDraft: typeof readLocalDraft;
    coalescingDraft_draft: ReturnType<typeof useSyncedDrafts>;
    coalescingDraft_db: Awaited<ReturnType<typeof syncDatabase>>["db"];
    coalescingDraft_root: import("react-dom/client").Root;
    coalescingDraft_outbox: ReturnType<
      typeof import("../../src/sync/send.ts").useOutbox
    >;
    coalescingDraft_lastRecoverySnapshot: {
      journal: string[];
      local: string | undefined;
    };
  }
}
import React from "react";
import { createRoot } from "react-dom/client";
export async function mount() {
  window.coalescingDraft_readDraft = readDraft;
  window.coalescingDraft_readLocalDraft = readLocalDraft;
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const { useOutbox } = await import("../../src/sync/send.ts");
  const { db } = await (
    await import("../../src/sync/client.ts")
  ).syncDatabase();
  window.coalescingDraft_db = db;
  const node = document.createElement("div");
  document.body.appendChild(node);
  window.coalescingDraft_root = createRoot(node);
  window.coalescingDraft_root.render(
    React.createElement(function Harness() {
      window.coalescingDraft_draft = useSyncedDrafts();
      window.coalescingDraft_outbox = useOutbox();
      return null;
    }),
  );
}
