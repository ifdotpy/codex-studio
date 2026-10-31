import { readDraft } from "./draft-fixture-helpers.ts";
declare global {
  interface Window {
    coalescingDraft_readDraft: typeof readDraft;
    root: import("react-dom/client").Root;
    outbox: ReturnType<typeof import("../../src/sync/send.ts").useOutbox>;
  }
}
import React from "react";
import { createRoot } from "react-dom/client";
export async function mount() {
  window.coalescingDraft_readDraft = readDraft;
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const { useOutbox } = await import("../../src/sync/send.ts");
  const { db } = await (
    await import("../../src/sync/client.ts")
  ).syncDatabase();
  window.db = db;
  const node = document.createElement("div");
  document.body.appendChild(node);
  window.root = createRoot(node);
  window.root.render(
    React.createElement(function Harness() {
      window.draft = useSyncedDrafts();
      window.outbox = useOutbox();
      return null;
    }),
  );
}
