function serverFrameHost(id) {
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
  return "studio-" + encoded.match(/.{1,50}/g).join(".") + ".localhost";
}
function frameOwner(url, assetsOrigin, shellOrigin) {
  try {
    const value = new URL(url);
    const base = new URL(assetsOrigin);
    const owner = value.searchParams.get("studio-server");
    if (
      !/^[a-zA-Z0-9_-]{1,128}$/.test(owner || "") ||
      value.pathname !== "/" ||
      value.hash ||
      value.username ||
      value.password
    )
      return;
    const host = serverFrameHost(owner);
    if (
      value.protocol !== base.protocol ||
      value.port !== base.port ||
      value.hostname !== host ||
      value.searchParams.get("studio-parent") !== shellOrigin
    )
      return;
    if (
      [...value.searchParams.keys()].some(
        (key) =>
          ![
            "studio-server",
            "studio-parent",
            "studio-origin",
            "studio-credential",
          ].includes(key),
      )
    )
      return;
    return owner;
  } catch {}
}
module.exports = { frameOwner, serverFrameHost };
