// Owns lowercase SHA-256 content identities for browser storage and requests.
export async function sha256Hex(input: Uint8Array): Promise<string> {
  const bytes = Uint8Array.from(input);
  const subtle = globalThis.crypto?.subtle;
  const digest =
    typeof subtle?.digest === "function"
      ? new Uint8Array(await subtle.digest("SHA-256", bytes))
      : await (await import("./wasm/client")).sha256(bytes);
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join(
    "",
  );
}
