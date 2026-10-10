import { expect, it } from "vitest";
import { defaultServerAlias, validateServerAlias } from "./serverAliases";
import { readServers, SERVER_REGISTRY_KEY } from "./registry";

it("uses the local default and derives general aliases from the host", () => {
  const row = (
    id: string,
    label: string,
    origin = "https://development-node.tailf00fa0.ts.net",
  ) => ({ id, label, origin });
  expect(defaultServerAlias(row("local", "This computer"))).toBe("LOC");
  expect(defaultServerAlias(row("peer", "Development node"))).toBe("DEV");
  expect(
    defaultServerAlias(
      row(
        "port",
        "Development node",
        "https://development-node.tailf00fa0.ts.net:8443",
      ),
    ),
  ).toBe("DEV");
  expect(
    defaultServerAlias(row("collision", "Same", "https://same.ts.net"), [
      "SAM",
    ]),
  ).toBe("AAA");
});

it("rejects empty, long, nonletter, lowercase, and duplicate aliases", () => {
  const storedAlias = ["M", "A", "C"].join("");
  for (const value of ["", "ABCD", "A1", "Ab", storedAlias])
    expect(() => validateServerAlias(value, [storedAlias])).toThrow();
  expect(validateServerAlias("A", [storedAlias])).toBe("A");
});

it("keeps explicit aliases and derives unique legacy defaults", () => {
  const storedAlias = ["M", "A", "C"].join("");
  const rows = ["one", "two", "three"].map((id) => ({
    id,
    label: id,
    origin: "https://same.tailnet.ts.net",
    credentialId: id,
    ...(id === "three" ? { alias: "SAM" } : {}),
    ...(id === "two" ? { alias: storedAlias } : {}),
  }));
  const storage = {
    getItem: (key: string) =>
      key === SERVER_REGISTRY_KEY ? JSON.stringify(rows) : null,
  } as Storage;
  expect(readServers(storage).map((row) => row.alias)).toEqual([
    "AAA",
    storedAlias,
    "SAM",
  ]);
});
