import type { SidebarBackend } from "../components/sidebar/services";
import type { ApiPostPath, PostBody, PostOptions, PostResult } from "../api";
import {
  preferenceStorageForServer,
  writePreferenceEdit,
} from "../sync/uiPreferenceStore";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import type { createSidebarRpcClient, SidebarPostPath } from "./sidebarRpc";

export function frameSidebarBackend(
  ownerId: string,
  rpc: ReturnType<typeof createSidebarRpcClient>,
  online: () => boolean,
): SidebarBackend {
  const storage = preferenceStorageForServer(ownerId);
  return {
    ownerId,
    storage,
    async post<Path extends ApiPostPath>(
      path: Path,
      body: PostBody<Path>,
      options?: PostOptions,
    ): Promise<PostResult<Path>> {
      if (!online()) throw new Error("The sidebar server is offline.");
      return (await rpc.request(ownerId, {
        action: "post",
        path: path as SidebarPostPath,
        body: body as unknown as PostBody<SidebarPostPath>,
        options: {
          timeoutMs: options?.timeoutMs,
          requestId: options?.requestId,
        },
      })) as PostResult<Path>;
    },
    saved<T>(key: string, fallback: T): T {
      try {
        const raw = storage.getItem(key);
        return raw === null ? fallback : JSON.parse(raw);
      } catch {
        return fallback;
      }
    },
    save(key, value) {
      storage.setItem(key, JSON.stringify(value));
    },
    editPreference(key, previous, next) {
      if (!online()) throw new Error("The sidebar server is offline.");
      return writePreferenceEdit(key, previous, next, storage);
    },
    subscribePreference(key, update) {
      let value = storage.getItem(key);
      const receive = () => {
        const next = storage.getItem(key);
        if (next === value) return;
        value = next;
        update();
      };
      window.addEventListener(preferenceEvent, receive);
      window.addEventListener("storage", receive);
      return () => {
        window.removeEventListener(preferenceEvent, receive);
        window.removeEventListener("storage", receive);
      };
    },
  };
}
