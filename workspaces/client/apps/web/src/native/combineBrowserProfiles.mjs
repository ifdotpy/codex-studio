/** Combine explicitly mapped source origins before the empty-destination import.
 * Existing server storage names are retained. Conflicts require reconciliation;
 * iteration order must never decide which user's saved data survives. */
export function combineBrowserProfiles(profiles) {
  if (!Array.isArray(profiles) || !profiles.some((row) => row.owner === "root"))
    throw new Error("The profile archive needs a root owner.");
  const owners = new Set();
  const preferences = Object.create(null);
  const databases = new Map();
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
      ![1, 2].includes(snapshot?.version) ||
      (version !== undefined && snapshot.version !== version) ||
      !snapshot.preferences ||
      typeof snapshot.preferences !== "object" ||
      Array.isArray(snapshot.preferences) ||
      !Array.isArray(snapshot.databases)
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
  if (profiles.length === 1) return profiles[0].snapshot;
  return {
    version,
    preferences,
    databases: [...databases.values()]
      .sort((a, b) =>
        a.database.name < b.database.name
          ? -1
          : a.database.name > b.database.name
            ? 1
            : 0,
      )
      .map(({ owner, database }) => ({
        ...database,
        stores: database.stores.map((store) => ({
          ...store,
          ...(store.chunks
            ? { chunks: store.chunks.map((chunk) => ({ ...chunk, owner })) }
            : {}),
        })),
      })),
  };
}
