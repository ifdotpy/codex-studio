import * as resources from "../../sync/subscriptions/events";
import { post, saved, save } from "../../api";
import { serverViewId } from "../../servers/environment";
import { serverLocalStorage } from "../../servers/storage";
import { preferenceEvent } from "../../sync/uiPreferenceMerge";
import {
  readPreferenceFields,
  receivePreferenceFields,
  writePreferenceEdit,
} from "../../sync/uiPreferenceStore";
import {
  createSidebarServices,
  useOptionalSidebarBackend,
  useSidebarVisualStateFor,
  type SidebarBackend,
} from "./serviceContext";
export * from "./serviceContext";

export const localSidebarBackend: SidebarBackend = {
  ownerId: serverViewId || "local",
  post,
  resources,
  saved,
  save,
  storage: serverLocalStorage,
  editPreference: writePreferenceEdit,
  preferenceFields: () => readPreferenceFields("server"),
  adoptPreferenceFields: (fields) => receivePreferenceFields("server", fields),
  subscribePreference(key, update) {
    const receive = (event: Event) => {
      if (event instanceof CustomEvent && event.detail !== key) return;
      if (event instanceof StorageEvent && event.key !== key) return;
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

export const localSidebarServices = createSidebarServices(
  localSidebarBackend,
  [localSidebarBackend],
  () => localSidebarBackend.ownerId,
);
export function useSidebarBackend() {
  return useOptionalSidebarBackend() ?? localSidebarBackend;
}
export function useSidebarVisualState<T>(key: string, fallback: T) {
  return useSidebarVisualStateFor(useSidebarBackend(), key, fallback);
}
