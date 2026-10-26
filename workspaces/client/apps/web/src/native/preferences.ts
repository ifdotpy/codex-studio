import { importBrowserProfile } from "./importBrowserProfile.ts";
import { combineBrowserProfiles } from "./combineBrowserProfiles.ts";
import { setNativeJournalStorage } from "./draftJournal";
import { hasNativeClient, nativeRequest } from "./client";
import { isNativeFrame } from "./frame";
import { serverViewId } from "../servers/environment";
import { guardNativeClose } from "./closeState";
import { NativePreferenceStorage, preferenceScope } from "./preferenceState";

export async function initializeNativePreferences() {
  if (!hasNativeClient()) return;
  const frame = isNativeFrame();
  const archive = frame
    ? await nativeRequest<{ id: string; snapshot: unknown } | null>(
        "browser.profile",
        {},
      )
    : await nativeRequest<{
        id: string;
        profiles: { owner: string; snapshot: unknown }[];
      } | null>("profile.workspace", {}).then(
        (workspace) =>
          workspace && {
            id: workspace.id,
            snapshot: combineBrowserProfiles(workspace.profiles),
          },
      );
  if (archive)
    await importBrowserProfile(
      archive.snapshot,
      archive.id,
      (chunk: { sha256: string; owner?: string }) =>
        nativeRequest(
          isNativeFrame() ? "browser.profileChunk" : "profile.chunk",
          isNativeFrame()
            ? { sha256: chunk.sha256 }
            : { owner: chunk.owner || "root", sha256: chunk.sha256 },
        ),
    );
  setNativeJournalStorage(window.localStorage);
  const invoke = isNativeFrame()
    ? <T>(method: string, params: unknown, id?: string) =>
        nativeRequest<T>("browser.preferences", { method, params }, id)
    : preferenceScope(nativeRequest, "root");
  const storage = await NativePreferenceStorage.open(
    window.localStorage,
    invoke,
    (error) => {
      window.dispatchEvent(
        new CustomEvent("desktop-error", {
          detail: `Settings are not saved yet. Keep Studio open. ${String(error)}`,
        }),
      );
    },
    isNativeFrame() ? `server:${serverViewId}` : "root",
  );
  Object.defineProperty(window, "localStorage", {
    value: storage,
    configurable: true,
  });
  guardNativeClose(async () => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        storage.flush(),
        new Promise<never>((_resolve, reject) => {
          timer = setTimeout(
            () =>
              reject(
                new Error(
                  "Settings have not finished saving. Keep Studio open.",
                ),
              ),
            8000,
          );
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  });
}
