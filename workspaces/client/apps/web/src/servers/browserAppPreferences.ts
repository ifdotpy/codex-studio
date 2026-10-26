import { createUiPreferenceBridge } from "../sync/uiPreferenceBridge";
import { writePreferenceEdit } from "../sync/uiPreferenceStore";
import { SERVER_REGISTRY_EVENT } from "./registry";
import type { syncPost } from "../api";
import type { AppPreferences } from "./appPreferences";

/** Browser shell metadata stays here. App never selects another storage owner. */
export function createBrowserAppPreferences(
  ownerId: string,
  storage: Storage,
  post: typeof syncPost,
): AppPreferences {
  return {
    editPreference: (key, previous, next) =>
      writePreferenceEdit(key, previous, next, storage),
    projectionBridge: createUiPreferenceBridge(storage, post),
    publishWorkspace(workspaceId) {
      const key = `studio-server-workspace-id:${ownerId}`;
      if (globalThis.localStorage.getItem(key) === workspaceId) return;
      globalThis.localStorage.setItem(key, workspaceId);
      window.dispatchEvent(new Event(SERVER_REGISTRY_EVENT));
    },
    announceStudioPreferences() {
      window.dispatchEvent(new Event("studio-preferences-change"));
    },
  };
}
