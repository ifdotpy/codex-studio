import { readDraft, readLocalDraft } from "./draft-fixture-helpers.ts";
import type { useSyncedDrafts } from "../../src/sync/drafts.ts";
import type { syncDatabase } from "../../src/sync/client.ts";
declare global {
  interface Window {
    recoveryDraft_readDraft: typeof readDraft;
    recoveryDraft_readLocalDraft: typeof readLocalDraft;
    recoveryDraft_draft: ReturnType<typeof useSyncedDrafts>;
    recoveryDraft_db: Awaited<ReturnType<typeof syncDatabase>>["db"];
    recoveryDraft_failWrites: boolean;
    recoveryDraft_blockDraftJournal: boolean;
    recoveryDraft_originalSetItem: Storage["setItem"];
  }
}
import React from "react";
import { createRoot } from "react-dom/client";
export async function mount(fail: boolean) {
  window.recoveryDraft_readDraft = readDraft;
  window.recoveryDraft_readLocalDraft = readLocalDraft;
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const { db } = await (
    await import("../../src/sync/client.ts")
  ).syncDatabase();
  window.recoveryDraft_db = db;
  window.recoveryDraft_failWrites = fail;
  const write = db.drafts.incrementalUpsert.bind(db.drafts);
  db.drafts.incrementalUpsert = (...args) =>
    window.recoveryDraft_failWrites
      ? Promise.reject(new Error("Write unavailable"))
      : write(...args);
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.recoveryDraft_draft = useSyncedDrafts();
      return null;
    }),
  );
}
