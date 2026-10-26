import type { SidebarBackend } from "../components/sidebar/services";
import type { ApiPostPath, PostBody, PostOptions, PostResult } from "../api";
import {
  readPreferenceFields,
  receivePreferenceFields,
  writePreferenceEdit,
} from "../sync/uiPreferenceStore";
import { preferenceEvent } from "../sync/uiPreferenceMerge";
import type { ServerServices } from "./serverServices";

export function serverSidebarBackend(
  services: ServerServices,
  online: () => boolean,
): SidebarBackend {
  const ownerId = services.owner.id;
  const storage = services.storage;
  return {
    ownerId,
    storage,
    resources: services.resources,
    async post<Path extends ApiPostPath>(
      path: Path,
      body: PostBody<Path>,
      options?: PostOptions,
    ): Promise<PostResult<Path>> {
      if (!online()) throw new Error("The sidebar server is offline.");
      await services.ready();
      return services.api.post(path, body, options);
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
    preferenceFields: () => readPreferenceFields("server", storage),
    adoptPreferenceFields: (fields) =>
      receivePreferenceFields("server", fields, storage),
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
