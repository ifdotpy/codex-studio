import type { BrowserProfileSnapshot } from "./importBrowserProfile.ts";
/** Combine explicitly mapped source origins before the empty-destination import.
 * Existing server storage names are retained. Conflicts require reconciliation;
 * iteration order must never decide which user's saved data survives. */
export function combineBrowserProfiles(
  profiles: { owner: string; snapshot: BrowserProfileSnapshot }[],
): BrowserProfileSnapshot;
export function combineBrowserProfiles(
  profiles: { owner: string; snapshot: unknown }[],
): unknown;
export function combineBrowserProfiles(
  profiles: { owner: string; snapshot: unknown }[],
): unknown {
  const record = (value: unknown): value is Record<string, unknown> =>
    typeof value === "object" && value !== null && !Array.isArray(value);
  const list = (value: unknown): value is unknown[] => Array.isArray(value);
  if (!Array.isArray(profiles) || !profiles.some((row) => row.owner === "root"))
    throw new Error("The profile archive needs a root owner.");
  const owners = new Set();
  const preferences: Record<string, unknown> = Object.create(null);
  const databases = new Map<
    string,
    { owner: string; database: Record<string, unknown> }
  >();
  let version;
  for (const { owner, snapshot } of profiles) {
    if (
      typeof owner !== "string" ||
      !(owner === "root" || /^server:[a-zA-Z0-9_-]+$/.test(owner)) ||
      owners.has(owner)
    )
      throw new Error("Invalid or duplicate profile owner.");
    owners.add(owner);
    if (
      !record(snapshot) ||
      (snapshot.version !== 1 && snapshot.version !== 2) ||
      (version !== undefined && snapshot.version !== version) ||
      !snapshot.preferences ||
      typeof snapshot.preferences !== "object" ||
      Array.isArray(snapshot.preferences) ||
      !list(snapshot.databases)
    )
      throw new Error("Invalid source profile contents.");
    version = snapshot.version;
    for (const [key, value] of Object.entries(snapshot.preferences)) {
      if (Object.hasOwn(preferences, key) && preferences[key] !== value)
        throw new Error(
          `Profile preference conflict: ${key}. The source profiles were not changed.`,
        );
      preferences[key] = value;
    }
    for (const database of snapshot.databases) {
      if (!record(database) || typeof database.name !== "string")
        throw new Error("Invalid source profile contents.");
      const previous = databases.get(database.name);
      if (
        previous &&
        JSON.stringify(previous.database) !== JSON.stringify(database)
      )
        throw new Error(
          `Profile database conflict: ${database.name}. The source profiles were not changed.`,
        );
      if (!previous) databases.set(database.name, { owner, database });
    }
  }
  // Preserve the existing receipt identity for archives that were root-only.
  if (profiles.length === 1) return profiles[0]?.snapshot;
  return {
    version,
    preferences,
    databases: [...databases.values()]
      .sort((a, b) =>
        String(a.database.name) < String(b.database.name)
          ? -1
          : String(a.database.name) > String(b.database.name)
            ? 1
            : 0,
      )
      .map(({ owner, database }) => {
        if (!list(database.stores))
          throw new TypeError("Invalid source profile contents.");
        return {
          ...database,
          stores: database.stores.map((store) => {
            if (!record(store))
              throw new Error("Invalid source profile contents.");
            if (store.chunks && !list(store.chunks))
              throw new TypeError("Invalid source profile contents.");
            return {
              ...store,
              ...(store.chunks
                ? {
                    chunks: (list(store.chunks) ? store.chunks : []).map(
                      (chunk) => {
                        if (!record(chunk))
                          throw new Error("Invalid source profile contents.");
                        return { ...chunk, owner };
                      },
                    ),
                  }
                : {}),
            };
          }),
        };
      }),
  };
}
