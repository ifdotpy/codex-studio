import { expect, it } from "vitest";
import { createDraftJournal } from "./draftJournal";
import { decodeDraftVersion } from "./draftDecoder";

it("rejects malformed conflict metadata while retaining the journal", () => {
  const key = "scope:pending:one";
  const raw = JSON.stringify({
    id: "id",
    session: "chat",
    device: "device",
    text: "draft",
    updated: 1,
    alternatives: 42,
  });
  const storage: Storage = {
    length: 1,
    key: () => key,
    getItem: () => raw,
    setItem: () => {
      throw new Error("unexpected write");
    },
    removeItem: () => {
      throw new Error("unexpected deletion");
    },
    clear: () => {
      throw new Error("unexpected deletion");
    },
  };
  expect(() => createDraftJournal(storage).readDraftJournal("scope")).toThrow();
  expect(storage.getItem(key)).toBe(raw);
});

it("validates optional metadata and unsigned timestamps", () => {
  const draft = {
    id: "id",
    session: "chat",
    device: "device",
    text: "draft",
    updated: 1,
    alternatives: ["other"],
    seen: { id: 0 },
  };
  expect(decodeDraftVersion(draft)).toEqual(draft);
  for (const value of [
    { ...draft, seen: { id: "bad" } },
    { ...draft, updated: -1 },
    { ...draft, updated: 1.5 },
  ])
    expect(() => decodeDraftVersion(value)).toThrow();
});
