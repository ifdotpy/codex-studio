import { expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ rerender: vi.fn() }));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: (value: unknown) => [value, mocks.rerender],
  useCallback: (callback: unknown) => callback,
}));
import { useUnavailableDrafts } from "./unavailableDrafts";

it("keeps typed text in memory and reports that it is not saved", () => {
  const drafts = useUnavailableDrafts();
  expect(drafts.getDraft("room")).toBe("");
  drafts.setDrafts({ room: "typed" });
  expect(drafts.getDraft("room")).toBe("typed");
  expect(drafts.localPersistenceFailed).toBe(true);
  expect(drafts.error).toContain("not saved");
  expect(drafts.conflicts).toEqual([]);
  expect(mocks.rerender).toHaveBeenCalledOnce();
});
