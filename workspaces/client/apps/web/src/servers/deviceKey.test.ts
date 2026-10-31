import { createPublicKey, verify } from "node:crypto";
import { expect, it } from "vitest";
import { createDeviceKey, deviceIdOf } from "./deviceKey";

it("makes a non-extractable key whose id is derived from its public key", async () => {
  const key = await createDeviceKey();
  expect(key.privateKey.extractable).toBe(false);
  expect(key.id).toMatch(/^dev-[0-9a-f]{64}$/);
  const spki = createPublicKey(key.publicKey).export({
    type: "spki",
    format: "der",
  });
  expect(await deviceIdOf(new Uint8Array(spki).buffer)).toBe(key.id);
  // The id fits the server's identifier rule (multi_server_auth::identifier).
  expect(key.id).toMatch(/^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$/);
});

it("gives two keys two ids and signs in the form the server verifies", async () => {
  const [first, second] = [await createDeviceKey(), await createDeviceKey()];
  expect(first.id).not.toBe(second.id);
  const message = new TextEncoder().encode("request");
  const signature = Buffer.from(
    await crypto.subtle.sign("Ed25519", first.privateKey, message),
  );
  expect(
    verify(null, message, createPublicKey(first.publicKey), signature),
  ).toBe(true);
});
