import { importBrowserProfile } from "./importBrowserProfile.ts";
import { combineBrowserProfiles } from "./combineBrowserProfiles.ts";
import { setNativeJournalStorage } from "./draftJournal";
import { hasNativeClient, nativeRequest } from "./client";
import { isNativeFrame } from "./frame";
import { serverViewId } from "../servers/environment";
import { guardNativeClose } from "./closeState";
import { NativePreferenceStorage, preferenceScope } from "./preferenceState";
import { installPreferenceStorage } from "../preferenceStorage";

// One-time read of the WebView storage that older builds wrote. Called only
// until the core holds the import marker.
function legacyBrowserEntries(): Record<string, string> {
  const source = globalThis.localStorage;
  return Object.fromEntries(
    Array.from({ length: source.length }, (_, index) => source.key(index)!).map(
      (key) => [key, source.getItem(key)!],
    ),
  );
}

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
  // The draft recovery journal stays in WebView storage until step 6 moves it.
  setNativeJournalStorage(globalThis.localStorage);
  const invoke = isNativeFrame()
    ? <T>(method: string, params: unknown, id?: string) =>
        nativeRequest<T>("browser.preferences", { method, params }, id)
    : preferenceScope(nativeRequest, "root");
  const storage = await NativePreferenceStorage.open(
    invoke,
    (error) => {
      window.dispatchEvent(
        new CustomEvent("desktop-error", {
          detail: `Settings are not saved yet. Keep Studio open. ${String(error)}`,
        }),
      );
    },
    legacyBrowserEntries,
    isNativeFrame() ? `server:${serverViewId}` : "root",
  );
  installPreferenceStorage(storage);
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
