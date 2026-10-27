import { beforeEach, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  localSubscribe: vi.fn(() => vi.fn()),
  slots: [] as unknown[],
  cursor: 0,
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useEffect: (effect: () => void | (() => void), dependencies: unknown[]) => {
    const slot = fixture.cursor++;
    const previous = fixture.slots[slot] as unknown[] | undefined;
    if (
      !previous ||
      previous.some((value, index) => value !== dependencies[index]) ||
      previous.length !== dependencies.length
    )
      fixture.effects.push(effect);
    fixture.slots[slot] = dependencies;
  },
  useRef: (current: unknown) => {
    const slot = fixture.cursor++;
    if (!(slot in fixture.slots)) fixture.slots[slot] = { current };
    return fixture.slots[slot];
  },
  useState: (current: unknown) => {
    const slot = fixture.cursor++;
    if (!(slot in fixture.slots)) fixture.slots[slot] = current;
    return [
      fixture.slots[slot],
      (value: unknown) => {
        fixture.slots[slot] =
          typeof value === "function" ? value(fixture.slots[slot]) : value;
      },
    ];
  },
}));
vi.mock("./ServerServicesContext", () => ({
  useServerSync: () => localSync,
  ServerServicesProvider: () => null,
}));
import { useServerFullAccounts } from "./ServerAccountLimits";
import type { ServerServices } from "./serverServices";
const localSync = { subscribeStateProjection: fixture.localSubscribe };
type Accept = Parameters<ServerServices["sync"]["subscribeStateProjection"]>[0];
function source() {
  const stop = vi.fn();
  let accept: Accept;
  const subscribe = vi.fn((next: Accept) => {
    accept = next;
    return stop;
  });
  return {
    service: {
      sync: { subscribeStateProjection: subscribe },
    } as unknown as ServerServices,
    subscribe,
    stop,
    publish: (id: string) =>
      accept!({
        runtime: { accountsSummary: { accounts: [{ id }] } },
      } as Parameters<Accept>[0]),
  };
}
const render = (
  selector: ((id: string) => ServerServices | undefined) | undefined,
) => {
  fixture.cursor = 0;
  return useServerFullAccounts(["remote"], selector)[0];
};
const effects = () => fixture.effects.splice(0).map((effect) => effect());
beforeEach(() => {
  fixture.effects.length = 0;
  fixture.localSubscribe.mockClear();
  fixture.slots = [];
  fixture.cursor = 0;
});

it("does not subscribe to the local owner for an unavailable remote service", () => {
  render(() => undefined);
  effects().forEach((stop) => stop?.());
  expect(fixture.localSubscribe).not.toHaveBeenCalled();
});

it("uses the local stream when no server selector exists", () => {
  render(undefined);
  effects().forEach((stop) => stop?.());
  expect(fixture.localSubscribe).toHaveBeenCalledTimes(1);
});

it("subscribes when the same selector gains a service", () => {
  const remote = source();
  let available = false;
  const select = () => (available ? remote.service : undefined);
  render(select);
  effects().forEach((stop) => stop?.());
  available = true;
  render(select);
  const stops = effects();
  remote.publish("new");
  expect(render(select)).toEqual({ remote: [{ id: "new" }] });
  stops.forEach((stop) => stop?.());
  expect(remote.subscribe).toHaveBeenCalledTimes(1);
  expect(fixture.localSubscribe).not.toHaveBeenCalled();
});

it("hides an old owner's accounts and ignores its callback after replacement", () => {
  const old = source(),
    next = source();
  let owner = old.service;
  const select = () => owner;
  render(select);
  const oldStops = effects();
  old.publish("old");
  expect(render(select)).toEqual({ remote: [{ id: "old" }] });
  expect(effects()).toEqual([]);
  owner = next.service;
  expect(render(select)).toEqual({});
  oldStops.forEach((stop) => stop?.());
  const nextStops = effects();
  next.publish("next");
  old.publish("late-old");
  expect(render(select)).toEqual({ remote: [{ id: "next" }] });
  nextStops.forEach((stop) => stop?.());
  expect(old.stop).toHaveBeenCalledTimes(1);
  expect(next.subscribe).toHaveBeenCalledTimes(1);
});
