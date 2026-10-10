import { describe, expect, it, vi } from "vitest";
import { refreshAfterCurrentPull } from "./refreshAfterCurrentPull";

describe("refresh after the current pull", () => {
  it("starts another pull after an older in-flight pull settles", async () => {
    let finishCurrent!: () => void;
    const currentPull = new Promise<void>((resolve) => {
      finishCurrent = resolve;
    });
    const refresh = vi.fn(async () => "fresh result");

    const reconciliation = refreshAfterCurrentPull(currentPull, refresh);
    expect(refresh).not.toHaveBeenCalled();

    finishCurrent();
    await expect(reconciliation).resolves.toBe("fresh result");
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it("starts another pull after the current one fails", async () => {
    const currentPull = Promise.reject(new Error("old pull failed"));
    const refresh = vi.fn(async () => "fresh result");

    await expect(refreshAfterCurrentPull(currentPull, refresh)).resolves.toBe(
      "fresh result",
    );
    expect(refresh).toHaveBeenCalledTimes(1);
  });
});
