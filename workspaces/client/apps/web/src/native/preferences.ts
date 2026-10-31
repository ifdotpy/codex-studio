import { importBrowserProfile } from "./importBrowserProfile.ts";
import { combineBrowserProfiles } from "./combineBrowserProfiles.ts";
import { hasNativeClient, nativeRequest } from "./client";
import { isNativeFrame } from "./frame";

export async function importNativeProfile() {
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
  await moveLegacyPreferences(
    (method, params) => nativeRequest(method, params),
    window.localStorage,
    isNativeFrame(),
  );
}

/**
 * Copies the preferences of the removed native store into the page storage, which
 * the browser path already syncs to the server. A native row is cleared only after
 * the page storage returns the same value. A key that already holds another value
 * keeps its native row. A failure keeps every row and the next start tries again.
 */
export async function moveLegacyPreferences(
  request: <T>(method: string, params: unknown) => Promise<T>,
  storage: Storage,
  frame: boolean,
) {
  try {
    const rows = await request<Record<string, string>>(
      frame ? "browser.legacyPreferences" : "preferences.legacy",
      frame ? {} : { scope: "root" },
    );
    const moved: string[] = [];
    for (const [key, raw] of Object.entries(rows)) {
      if (storage.getItem(key) === null) storage.setItem(key, raw);
      if (storage.getItem(key) === raw) moved.push(key);
    }
    if (moved.length)
      await request(
        frame ? "browser.legacyPreferencesClear" : "preferences.legacy_clear",
        frame ? { keys: moved } : { scope: "root", keys: moved },
      );
  } catch (error) {
    console.error("Old desktop preferences were not moved.", error);
  }
}
