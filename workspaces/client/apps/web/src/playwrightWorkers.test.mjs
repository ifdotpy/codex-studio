import { expect, test } from "vitest";
import { darwinAvailableBytes } from "../playwright-workers.mjs";

test("macOS memory comes from the kernel's available percent", () => {
  const total = 128 * 1024 ** 3;
  expect(darwinAvailableBytes("86\n", total)).toBe(Math.floor(total * 0.86));
  expect(darwinAvailableBytes(0, total)).toBe(0);
  expect(darwinAvailableBytes("100", total)).toBe(total);
  for (const bad of ["", "-5", "101", "8.5", "high", undefined])
    expect(darwinAvailableBytes(bad, total)).toBeUndefined();
});
