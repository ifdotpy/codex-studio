import { describe, expect, it, vi } from "vitest";
import { limitsBaselineReader } from "./limitsBaseline";

describe("limits baseline hydration", () => {
  it("runs one fresh read when cache hydration rejects and shows its limits", async () => {
    let shownLimits: number | undefined;
    const requests = { cached: 0, fresh: 0 };
    let hydrated = false;
    requests.cached++;
    const freshRead = vi.fn(async () => {
      requests.fresh++;
      shownLimits = 42;
    });
    await Promise.reject(new Error("cache unavailable")).catch(() => {
      hydrated = false;
    });
    const onBaseline = limitsBaselineReader(() => hydrated, freshRead);

    await onBaseline();

    expect(freshRead).toHaveBeenCalledTimes(1);
    expect(requests).toEqual({ cached: 1, fresh: 1 });
    expect(shownLimits).toBe(42);
  });

  it("uses a successfully hydrated cache for the baseline", async () => {
    const requests = { cached: 0, fresh: 0 };
    requests.cached++;
    const freshRead = vi.fn(async () => {});
    const onBaseline = limitsBaselineReader(() => true, freshRead);

    await onBaseline();

    expect(freshRead).not.toHaveBeenCalled();
    expect(requests).toEqual({ cached: 1, fresh: 0 });
  });
});
