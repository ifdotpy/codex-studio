export interface LocalFileLink {
  path: string;
  line?: number;
}

// Keep URL schemes explicit. Local paths still go through the server's workspace checks.
export const markdownUriPattern =
  /^(?:(?:https?|mailto|tel|sms|cid|xmpp|file|sandbox):|[^a-z]|[a-z+.\-]+(?:[^a-z+.\-:]|$))/i;

export function localFileLink(href: string): LocalFileLink | null {
  const value = href.trim();
  if (!value || value.startsWith("#") || value.startsWith("//")) return null;
  if (
    /^[a-z][a-z\d+.-]*:/i.test(value) &&
    !/^(file|sandbox):/i.test(value) &&
    !relativeFileLocation(value)
  )
    return null;
  let path = value;
  if (/^file:/i.test(path)) {
    const url = new URL(path);
    if (url.hostname && url.hostname !== "localhost")
      throw new Error("Remote file links are not supported.");
    path = url.pathname + url.hash;
  } else if (/^sandbox:/i.test(path)) {
    path = path.slice("sandbox:".length);
    if (!path.startsWith("/") || path.startsWith("//"))
      throw new Error("This sandbox file link is invalid.");
  }
  const fragment = path.indexOf("#");
  const anchor = fragment < 0 ? "" : path.slice(fragment + 1);
  path = fragment < 0 ? path : path.slice(0, fragment);
  path = decodeURIComponent(path);
  const location = path.match(/:(\d+)(?::\d+)?$/);
  if (location) path = path.slice(0, location.index);
  const line = Number(
    anchor.match(/^L?(\d+)(?:C\d+|-L?\d+)?$/i)?.[1] || location?.[1],
  );
  if (!path || /[\u0000-\u001f]/.test(path))
    throw new Error("This file link is invalid.");
  return { path, ...(Number.isSafeInteger(line) && line > 0 ? { line } : {}) };
}

// A bare filename with a line suffix otherwise resembles a custom URL scheme.
export function relativeFileLocation(value: string) {
  return /^[^/:?#]+\.[^/:?#]+:\d+(?::\d+)?(?:#.*)?$/.test(value);
}
