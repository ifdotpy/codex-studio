import { expect, it } from "vitest";
import { defaultServerAlias, validateServerAlias } from "./serverAliases";
import { readServers, SERVER_REGISTRY_KEY } from "./registry";

it("derives the four approved aliases from identity and origin", () => {
  const row = (
    id: string,
    label: string,
    origin = "https://kukuka-win.tailf00fa0.ts.net",
  ) => ({ id, label, origin });
  expect(defaultServerAlias(row("local", "This computer"))).toBe("MAC");
  expect(defaultServerAlias(row("mbp", "igor-mbp"))).toBe("MBP");
  expect(defaultServerAlias(row("wsl", "kukuka-win"))).toBe("WSL");
  expect(
    defaultServerAlias(
      row("win", "kukuka-win", "https://kukuka-win.tailf00fa0.ts.net:8443"),
    ),
  ).toBe("WIN");
  expect(defaultServerAlias(row("new", "kukuka-win"), ["WSL"])).toBe("AAA");
});

it("rejects empty, long, nonletter, lowercase, and duplicate aliases", () => {
  for (const value of ["", "ABCD", "A1", "Ab", "MAC"])
    expect(() => validateServerAlias(value, ["MAC"])).toThrow();
  expect(validateServerAlias("A", ["MAC"])).toBe("A");
});

it("keeps explicit aliases and derives unique legacy defaults", () => {
  const rows = ["one", "two", "three"].map((id) => ({
    id,
    label: id,
    origin: "https://same.tailnet.ts.net",
    credentialId: id,
    ...(id === "three" ? { alias: "SAM" } : {}),
  }));
  const storage = {
    getItem: (key: string) =>
      key === SERVER_REGISTRY_KEY ? JSON.stringify(rows) : null,
  } as Storage;
  expect(readServers(storage).map((row) => row.alias)).toEqual([
    "AAA",
    "AAB",
    "SAM",
  ]);
});
