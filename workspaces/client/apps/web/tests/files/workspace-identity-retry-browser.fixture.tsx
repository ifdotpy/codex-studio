declare global {
  interface Window {
    workspaceIdentityRetryBrowserWorkspace: string | undefined;
    workspaceIdentityRetryBrowserMount: typeof mount;
  }
}
window.workspaceIdentityRetryBrowserMount = mount;

import React from "react";
import { createRoot } from "react-dom/client";
import { useSnapshot } from "../../src/hooks.ts";
export function mount() {
  createRoot(document.body).render(
    React.createElement(function Harness() {
      window.workspaceIdentityRetryBrowserWorkspace = useSnapshot().workspaceId;
      return null;
    }),
  );
}
