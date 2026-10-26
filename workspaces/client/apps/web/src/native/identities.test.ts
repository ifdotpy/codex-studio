import { expect, it, vi } from "vitest";
import { createNativeIdentityPool } from "./identities";

const id = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
it("consumes native identities once and coalesces asynchronous refill", async () => {
  let finish!: (value: { ids: string[]; refillAt: number }) => void;
  const load = vi.fn().mockRejectedValue(new Error("No further fixture batch.")).mockResolvedValueOnce({ ids: [id(1), id(2)], refillAt: 1 }).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
  const pool = createNativeIdentityPool(load, vi.fn());
  await pool.initialize();
  expect(pool.next()).toBe(id(1));
  expect(pool.next()).toBe(id(2));
  expect(() => pool.next()).toThrow("Wait");
  expect(load).toHaveBeenCalledTimes(2);
  finish({ ids: [id(3), id(4)], refillAt: 1 });
  await pool.initialize();
  expect(pool.next()).toBe(id(3));
});

it("refuses duplicated identities instead of creating a browser fallback", async () => {
  const report = vi.fn();
  const pool = createNativeIdentityPool(async () => ({ ids: [id(1), id(1)], refillAt: 0 }), report);
  await expect(pool.initialize()).rejects.toThrow("duplicate");
  expect(() => pool.next()).toThrow("Wait");
});
