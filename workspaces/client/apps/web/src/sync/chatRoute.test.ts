import { expect, it } from "vitest";
import { readChatRoute } from "./chatRoute";

it("reads only explicit chat selections from the URL", () => {
  expect(readChatRoute("")).toBeNull();
  expect(readChatRoute("?chat=alpha")).toEqual({ id: "alpha", kind: "agent" });
  expect(readChatRoute("?chat=alpha&chatKind=room")).toEqual({
    id: "alpha",
    kind: "room",
  });
  expect(readChatRoute("?chat=alpha&chatKind=legacy")).toEqual({
    id: "alpha",
    kind: "legacy",
  });
});
