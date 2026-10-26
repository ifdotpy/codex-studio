// A server view has an immutable owner for its complete lifetime. A late
// response or queued send cannot acquire the owner of a newly selected view.
export const serverViewId =
  typeof location === "undefined"
    ? null
    : new URLSearchParams(location.search).get("studio-server");
export const isServerView = !!serverViewId;
export const isClassicServerView =
  isServerView &&
  new URLSearchParams(location.search).get("studio-navigation") === "classic";
export const isRemoteServerView = !!serverViewId && serverViewId !== "local";
export const storageNameForServer = (owner: string | null, name: string) =>
  owner && owner !== "local" ? `${name}:server:${owner}` : name;
export const serverStorageName = (name: string) =>
  storageNameForServer(serverViewId, name);

export const databaseSuffixForServer = (owner: string | null) =>
  owner && owner !== "local"
    ? "server" +
      Array.from(new TextEncoder().encode(owner), (byte) =>
        byte.toString(16).padStart(2, "0"),
      ).join("")
    : "";
export const serverDatabaseSuffix = databaseSuffixForServer(serverViewId);

export const serverParentOrigin =
  typeof location === "undefined"
    ? "http://localhost"
    : new URLSearchParams(location.search).get("studio-parent") ||
      location.origin;
export const isolatedServerView =
  !!serverViewId &&
  typeof location !== "undefined" &&
  location.origin !== serverParentOrigin;
