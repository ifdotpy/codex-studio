import { beforeEach, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
  owner: 0,
  effects: [] as Array<() => void | (() => void)>,
  readers: [vi.fn(), vi.fn()],
  writes: [vi.fn(), vi.fn()],
  subscribe: [vi.fn(), vi.fn()],
  stops: [vi.fn(), vi.fn()],
  changed: [] as Array<(key: string | null) => void>,
  render: vi.fn(),
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    h.render,
  ],
  useEffect: (effect: () => void | (() => void)) => h.effects.push(effect),
}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ saved: h.readers[h.owner] }),
  useServerLocalDrafts: () => ({ writeLocalDraft: h.writes[h.owner] }),
  useServerPreferenceEvents: () => ({ subscribe: h.subscribe[h.owner] }),
}));
vi.mock("../api", () => {
  throw new Error("The native caller imported the browser API.");
});
vi.mock("../sync/localDraft", () => {
  throw new Error("The native caller imported the browser draft owner.");
});

import { useFormDraft } from "../components/useFormDraft";
import { useSyncedVisualState } from "../sync/useSyncedVisualState";

beforeEach(() => {
  h.owner = 0;
  h.effects = [];
  h.changed = [];
  h.render.mockReset();
  for (const index of [0, 1]) {
    h.readers[index].mockReset().mockReturnValue({ owner: index });
    h.writes[index].mockReset().mockReturnValue(null);
    h.stops[index].mockReset();
    h.subscribe[index].mockReset().mockImplementation((listener) => {
      h.changed[index] = listener;
      return h.stops[index];
    });
  }
});

it("a form draft reads and writes only its captured server owner", () => {
  const notify = vi.fn();
  const [value, update] = useFormDraft("profile", notify);
  expect(value).toEqual({ owner: 0 });
  h.owner = 1;
  update({ value: "first" });
  expect(h.writes[0]).toHaveBeenCalledExactlyOnceWith("profile", {
    value: "first",
  });
  expect(h.writes[1]).not.toHaveBeenCalled();
  h.writes[0].mockReturnValue("The draft could not be saved.");
  update({ value: "second" });
  expect(notify).toHaveBeenCalledExactlyOnceWith(
    "The draft could not be saved.",
  );
});

it("visual preferences subscribe to each captured owner and release its exact listener", () => {
  useSyncedVisualState("disclosures", {});
  h.owner = 1;
  useSyncedVisualState("disclosures", {});
  const cleanups = h.effects.map((effect) => effect());
  for (const index of [0, 1]) {
    expect(h.subscribe[index]).toHaveBeenCalledOnce();
    const before = h.readers[index].mock.calls.length;
    h.changed[index]("another-key");
    expect(h.readers[index]).toHaveBeenCalledTimes(before);
    h.changed[index]("disclosures");
    expect(h.readers[index]).toHaveBeenCalledTimes(before + 1);
    h.changed[index](null);
    expect(h.readers[index]).toHaveBeenCalledTimes(before + 2);
  }
  for (const cleanup of cleanups) cleanup?.();
  expect(h.stops[0]).toHaveBeenCalledOnce();
  expect(h.stops[1]).toHaveBeenCalledOnce();
});
