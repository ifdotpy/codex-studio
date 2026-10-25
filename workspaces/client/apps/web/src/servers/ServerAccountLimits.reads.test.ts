import { beforeEach, expect, it, vi } from "vitest";

const fixture = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  localGet: vi.fn(async () => ({ accounts: [] })),
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
  useServerApi: () => ({ get: fixture.localGet }),
  ServerServicesProvider: () => null,
}));
import { useServerFullAccounts } from "./ServerAccountLimits";
import { createServerServices } from "./serverServices";
beforeEach(() => {
  fixture.effects.length = 0;
  fixture.localGet.mockClear();
  fixture.slots = [];
  fixture.cursor = 0;
});

it("does not send a remote account read through the local fallback API", async () => {
  useServerFullAccounts(["remote"], () => undefined);
  const stops = fixture.effects.map((effect) => effect());
  await Promise.resolve();
  for (const stop of stops) stop?.();
  expect(fixture.localGet).not.toHaveBeenCalled();
});

it("keeps the local-only account read when no server service selector exists", async () => {
  useServerFullAccounts(["local"], undefined);
  const stops = fixture.effects.map((effect) => effect());
  await Promise.resolve();
  for (const stop of stops) stop?.();
  expect(fixture.localGet).toHaveBeenCalledExactlyOnceWith("/api/accounts");
});

it("reads a service that becomes available with the same server IDs and selector", async () => {
  const service = createServerServices({
    id: "remote",
    label: "Remote",
    origin: "https://remote.test",
  });
  const get = vi
    .spyOn(service.api, "get")
    .mockRejectedValue(new Error("fixture account read unavailable"));
  const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
  let available = false;
  const select = () => (available ? service : undefined);
  useServerFullAccounts(["remote"], select);
  const stops = fixture.effects.splice(0).map((effect) => effect());
  available = true;
  fixture.cursor = 0;
  useServerFullAccounts(["remote"], select);
  const resumed = fixture.effects.splice(0).map((effect) => effect());
  await Promise.resolve();
  await Promise.resolve();
  for (const stop of [...stops, ...resumed]) stop?.();
  service.close();
  expect(get).toHaveBeenCalledExactlyOnceWith("/api/accounts");
  expect(fixture.localGet).not.toHaveBeenCalled();
  warning.mockRestore();
});
