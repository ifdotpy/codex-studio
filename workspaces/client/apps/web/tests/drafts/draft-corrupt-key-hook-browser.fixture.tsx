import type {} from "./draft-fixture-helpers.ts";
import React from "react";
import { createRoot } from "react-dom/client";
export async function mount() {
  const { useSyncedDrafts } = await import("../../src/sync/drafts.ts");
  const node = document.createElement("div");
  document.body.appendChild(node);
  createRoot(node).render(
    React.createElement(function Harness() {
      window.draft = useSyncedDrafts();
      return null;
    }),
  );
}
