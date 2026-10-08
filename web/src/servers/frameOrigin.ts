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
  const base = new URL(assets || root.origin);
  const isolated =
    !!assets ||
    (base.protocol === "http:" &&
      ["127.0.0.1", "localhost"].includes(base.hostname));
  if (isolated) {
    base.hostname = serverFrameHost(server.id);
  }
  base.pathname = "/";
  base.search = new URLSearchParams({
    "studio-server": server.id,
    ...(isolated
      ? {
          "studio-parent": root.origin,
          "studio-origin": server.origin,
          "studio-credential": server.credentialId || "",
        }
      : {}),
  }).toString();
  return base.href;
}
