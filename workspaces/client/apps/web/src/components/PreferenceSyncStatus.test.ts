import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, it } from "vitest";
import PreferenceSyncStatus from "./PreferenceSyncStatus";
import { setPreferenceWriteStatus } from "../sync/uiPreferenceStatus";

function storage(): Storage {
  return {
    length: 0,
    key: () => null,
    getItem: () => null,
    setItem() {},
    removeItem() {},
    clear() {},
  };
}

it("shows persistent uncertainty only for its exact owner and scope", () => {
  const first = storage();
  const second = storage();
  setPreferenceWriteStatus(first, "user", {
    state: "error",
    error: "The reply was lost.",
    uncertain: true,
    requestId: "original",
  });
  const render = (value: Storage) =>
    renderToStaticMarkup(
      createElement(PreferenceSyncStatus, { storage: value, scope: "user" }),
    );
  expect(render(first)).toContain('role="alert"');
  expect(render(first)).toContain("The reply was lost.");
  expect(render(first)).toContain("The server has not confirmed this request.");
  expect(render(second)).toBe("");
  expect(render(first)).toContain("The reply was lost.");
});
