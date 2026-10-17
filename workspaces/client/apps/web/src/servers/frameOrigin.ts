import type { StudioServer } from "./registry";
export function serverFrameHost(id: string): string {
  const alphabet = "abcdefghijklmnopqrstuvwxyz234567";
  let bits = 0,
    value = 0,
    encoded = "";
  for (const byte of new TextEncoder().encode(id)) {
    value = (value << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      encoded += alphabet[(value >>> bits) & 31];
    }
  }
  if (bits) encoded += alphabet[(value << (5 - bits)) & 31];
  return "studio-" + encoded.match(/.{1,50}/g)!.join(".") + ".localhost";
}
export function frameURL(
  server: StudioServer,
  shell = location.href,
  assets = window.codexDesktop?.serverViewOrigin,
): string {
  const root = new URL(shell);
  const native = root.protocol === "tauri:";
  const base = new URL(
    native ? "studio-view://localhost" : assets || root.origin,
  );
  const isolated =
    native ||
    !!assets ||
    (base.protocol === "http:" &&
      ["127.0.0.1", "localhost"].includes(base.hostname));
  if (isolated) {
    base.hostname = serverFrameHost(server.id);
  }
  base.pathname = "/";
  base.search = new URLSearchParams({
    "studio-server": server.id,
    "studio-navigation":
      root.searchParams.get("studio-navigation") === "classic"
        ? "classic"
        : "combined",
    ...(isolated
      ? {
          "studio-parent": native ? "tauri://localhost" : root.origin,
          "studio-origin": server.origin,
          "studio-credential": server.credentialId || "",
        }
      : {}),
  }).toString();
  return base.href;
}

// WebKit assigns registered native schemes a host origin even though the URL
// standard reports "null". Message destinations must match MessageEvent.origin.
export function frameMessageOrigin(value: string): string {
  const url = new URL(value);
  return url.protocol === "studio-view:" || url.protocol === "tauri:"
    ? `${url.protocol}//${url.host}`
    : url.origin;
}
