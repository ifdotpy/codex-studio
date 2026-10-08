import { describe, expect, it, vi } from "vitest";
import { limitsBaselineReader } from "./limitsBaseline";

describe("limits baseline hydration", () => {
  it("runs one fresh read when cache hydration rejects and shows its limits", async () => {
    let shownLimits: number | undefined;
    const requests = { cached: 0, fresh: 0 };
    const hydrate = vi.fn(async () => {
      requests.cached++;
      throw new Error("cache unavailable");
    });
    const freshRead = vi.fn(async () => {
      requests.fresh++;
      shownLimits = 42;
    });
    const onBaseline = limitsBaselineReader(hydrate, freshRead);

    await onBaseline();

    expect(freshRead).toHaveBeenCalledTimes(1);
    expect(requests).toEqual({ cached: 1, fresh: 1 });
    expect(shownLimits).toBe(42);
  });

  it("awaits successful hydration and issues no extra read on baseline", async () => {
    const requests = { cached: 0, fresh: 0 };
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const hydrate = vi.fn(() => {
      requests.cached++;
      return hydration;
    });
    const freshRead = vi.fn(async () => {
      requests.fresh++;
    });
    const onBaseline = limitsBaselineReader(hydrate, freshRead);

    const baseline = onBaseline();
    expect(requests).toEqual({ cached: 1, fresh: 0 });
    resolveHydration(true);
    await baseline;

    expect(hydrate).toHaveBeenCalledTimes(1);
    expect(freshRead).not.toHaveBeenCalled();
    expect(requests).toEqual({ cached: 1, fresh: 0 });

    await onBaseline();
    expect(freshRead).toHaveBeenCalledTimes(1);
    expect(requests).toEqual({ cached: 1, fresh: 1 });
  });

  it("does not fall back if the account disconnects during hydration", async () => {
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const read = vi.fn(async () => {});
    let connected = true;
    const onBaseline = limitsBaselineReader(
      () => hydration,
      read,
      () => connected,
    );

    const baseline = onBaseline();
    connected = false;
    resolveHydration(false);
    await baseline;

    expect(read).not.toHaveBeenCalled();
  });

  it("does not read after disposal while failed hydration is pending", async () => {
    let resolveHydration!: (result: boolean) => void;
    const hydration = new Promise<boolean>((resolve) => {
      resolveHydration = resolve;
    });
    const read = vi.fn(async () => {});
    let active = true;
    const onBaseline = limitsBaselineReader(
      () => hydration,
      read,
      () => active,
    );

    const baseline = onBaseline();
    active = false;
    resolveHydration(false);
    await baseline;

    expect(read).not.toHaveBeenCalled();
  });
});
