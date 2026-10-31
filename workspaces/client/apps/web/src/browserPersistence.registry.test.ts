import { expect, it, vi } from "vitest";
import {
  readPairedServerReferences,
  writePairedServerReferences,
} from "./browserPersistence";

it("has no paired servers when browser storage is unavailable", () => {
  vi.stubGlobal("localStorage", undefined);
  try {
    expect(readPairedServerReferences()).toEqual([]);
  } finally {
    vi.unstubAllGlobals();
  }
});

it("keeps only connection identities when rewriting legacy paired servers", () => {
  const rows = [
    {
      id: "server",
      origin: "https://server.tailnet.ts.net",
      credentialId: "credential",
      label: "Old label",
      alias: "OLD",
    },
  ];
  let saved = JSON.stringify(rows);
  const storage = {
    length: 1,
    clear: () => {
      saved = "";
    },
    getItem: () => saved,
    key: () => null,
    removeItem: () => {
      saved = "";
    },
    setItem: (_key: string, value: string) => {
      saved = value;
    },
  } as Storage;
  const references = readPairedServerReferences(storage);
  expect(references).toEqual([
    {
      id: "server",
      origin: "https://server.tailnet.ts.net",
      credentialId: "credential",
    },
  ]);
  writePairedServerReferences(references, storage);
  expect(JSON.parse(saved)).toEqual(references);
});
