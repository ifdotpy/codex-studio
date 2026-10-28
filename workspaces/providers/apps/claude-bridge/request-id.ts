// Owns UUIDv7 issuance for Studio callers. Persist the value before dispatch.
export function requestId(): string {
  const crypto = globalThis.crypto;
  if (typeof crypto?.getRandomValues !== "function") {
    throw new Error("Secure random values are unavailable for UUID creation");
  }

  const bytes = crypto.getRandomValues(new Uint8Array(16));
  // The array has 16 bytes; the defaults only satisfy indexed-access typing.
  let issued = Date.now();
  for (let index = 5; index >= 0; index--) {
    bytes[index] = issued % 256;
    issued = Math.floor(issued / 256);
  }
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x70;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0"));
  return `${hex.slice(0, 4).join("")}-${hex.slice(4, 6).join("")}-${hex.slice(6, 8).join("")}-${hex.slice(8, 10).join("")}-${hex.slice(10).join("")}`;
}
