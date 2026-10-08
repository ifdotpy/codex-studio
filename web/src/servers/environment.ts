// A server view has an immutable owner for its complete lifetime. A late
// response or queued send cannot acquire the owner of a newly selected view.
export const serverViewId =
  typeof location === "undefined"
    ? null
    : new URLSearchParams(location.search).get("studio-server");
export const isServerView = !!serverViewId;
export const isRemoteServerView = !!serverViewId && serverViewId !== "local";
export const serverStorageName = (name: string) =>
  serverViewId && serverViewId !== "local"
    ? `${name}:server:${serverViewId}`
    : name;

export const serverDatabaseSuffix =
  serverViewId && serverViewId !== "local"
    ? "server" +
      Array.from(new TextEncoder().encode(serverViewId), (byte) =>
        byte.toString(16).padStart(2, "0"),
      ).join("")
    : "";
