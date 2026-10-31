import { expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  write: vi.fn(),
  saved: vi.fn(() => ({ old: true })),
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: () => [0, vi.fn()],
}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ saved: mocks.saved }),
}));
vi.mock("../sync/localDraft", () => ({ writeLocalDraft: mocks.write }));
import { useFormDraft } from "./useFormDraft";

it("keeps form drafts in memory without reading or writing browser storage", () => {
  const [initial, update] = useFormDraft("form");
  expect(initial).toBeNull();
  update({ text: "edited" });
  expect(mocks.saved).not.toHaveBeenCalled();
  expect(mocks.write).not.toHaveBeenCalled();
});
