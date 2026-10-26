import { readDraft, readLocalDraft } from "./draft-fixture-helpers.ts";
declare global {
  interface Window {
    recoveryDraft_readDraft: typeof readDraft;
    recoveryDraft_readLocalDraft: typeof readLocalDraft;
    failWrites: boolean;
    blockDraftJournal: boolean;
    originalSetItem: Storage["setItem"];
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
  window.db = db;
  window.failWrites = fail;
  const write = db.drafts.incrementalUpsert.bind(db.drafts);
  db.drafts.incrementalUpsert = (...args) =>
    window.failWrites
      ? Promise.reject(new Error("Write unavailable"))
      : write(...args);
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.draft = useSyncedDrafts();
      return null;
    }),
  );
}
