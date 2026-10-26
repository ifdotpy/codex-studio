import { expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  fallback: vi.fn(() => vi.fn()),
}));
vi.mock("react", () => ({
  useState: (value: unknown) => [value, vi.fn()],
  useRef: (current: unknown) => ({ current }),
  useEffect: (effect: () => void | (() => void)) => {
    h.effects.push(effect);
  },
}));
vi.mock("./ServerServicesContext", () => ({
  useServerSync: () => ({ subscribeStateProjection: h.fallback }),
}));
vi.mock("../apiPresentation", () => ({ errorText: String }));
vi.mock("../api", () => ({ errorText: String }));
vi.mock("../components/AccountStartThrottle", () => ({ default: () => null }));
vi.mock("../components/AccountGroup", () => ({ default: () => null }));
vi.mock("../components/AccountTiles", () => ({ accountLimitView: vi.fn() }));
vi.mock("../components/Accounts", () => ({ AccountCapacityView: () => null }));
vi.mock("../usage/useAccountLimits", () => ({ useAccountLimits: vi.fn() }));
import { useServerFullAccounts } from "./ServerAccountLimits";

it("never falls back to another server while an explicit native service is opening", () => {
  useServerFullAccounts(["remote"], () => undefined);
  for (const effect of h.effects.splice(0)) effect();
  expect(h.fallback).not.toHaveBeenCalled();
  const watchAccounts = vi.fn(() => vi.fn());
  useServerFullAccounts(["remote"], () => ({
    watchAccounts,
    renderScope: (children) => children,
  }));
  for (const effect of h.effects.splice(0)) effect();
  expect(watchAccounts).toHaveBeenCalledOnce();
  expect(h.fallback).not.toHaveBeenCalled();
});
