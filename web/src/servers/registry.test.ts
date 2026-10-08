import { expect, it } from "vitest";
import { readServers, serveOrigin, SERVER_REGISTRY_KEY } from "./registry";
it("accepts only a Tailscale Serve HTTPS origin without credentials or paths", () => {
  expect(serveOrigin("https://computer.tailnet.ts.net/")).toBe(
    "https://computer.tailnet.ts.net",
  );
  for (const address of [
    "http://computer.tailnet.ts.net",
    "https://public.example",
    "https://name.ts.net.attacker.test",
    "https://user:secret@computer.tailnet.ts.net",
    "https://computer.tailnet.ts.net:444",
    "https://computer.tailnet.ts.net/path",
    "https://computer.tailnet.ts.net/?key=secret",
  ])
    expect(() => serveOrigin(address)).toThrow();
});
it("validates saved identities and never loads local rows as paired servers", () => {
  const storage = (value: unknown) =>
    ({
      getItem: (key: string) =>
        key === SERVER_REGISTRY_KEY ? JSON.stringify(value) : null,
    }) as Storage;
  const row = {
    id: "server-1",
    label: "Server",
    origin: "https://computer.tailnet.ts.net",
    credentialId: "device-1",
  };
  expect(readServers(storage([row]))).toEqual([row]);
  for (const invalid of [
    [row, row],
    [{ ...row, id: "local" }],
    [{ ...row, credentialId: undefined }],
    [{ ...row, id: "../other" }],
  ])
    expect(() => readServers(storage(invalid))).toThrow("invalid");
});
