import { describe, expect, it, vi } from "vitest";
import { createQueueResourceRefresh } from "./queueResourceRefresh";

describe("queue resource refresh", () => {
  it("drains an invalidation received during the final mutation read after unlock", async () => {
    let locked = true;
    const refresh = vi.fn(async () => {});
    const gate = createQueueResourceRefresh(() => locked, refresh);
    let finishFinalRead!: () => void;
    const finalRead = new Promise<void>((resolve) => {
      finishFinalRead = resolve;
    });

    const finalReadInFlight = finalRead;
    await gate.invalidate();
    expect(refresh).not.toHaveBeenCalled();

    finishFinalRead();
    await finalReadInFlight;
    locked = false;
    await gate.flush();

    expect(refresh).toHaveBeenCalledOnce();
  });

  it("reads immediately when no mutation owns the queue", async () => {
    const refresh = vi.fn(async () => {});
    const gate = createQueueResourceRefresh(() => false, refresh);

    await gate.invalidate();

    expect(refresh).toHaveBeenCalledOnce();
  });
});
