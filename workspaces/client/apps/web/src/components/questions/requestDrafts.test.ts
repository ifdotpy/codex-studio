import { expect, it } from "vitest";
import { requestDraftsForStorage } from "./requestDrafts";

it("keeps matching request IDs and secret answers with their storage owner", () => {
  const first = {} as Storage;
  const second = {} as Storage;
  const a = requestDraftsForStorage(first);
  const b = requestDraftsForStorage(second);
  a.set("same-request", { secret: "first secret" });
  b.set("same-request", { secret: "second secret" });
  expect(requestDraftsForStorage(first).get("same-request")).toEqual({
    secret: "first secret",
  });
  a.delete("same-request");
  expect(b.get("same-request")).toEqual({ secret: "second secret" });
  expect(requestDraftsForStorage({} as Storage).size).toBe(0);
});
