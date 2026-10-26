import { expect, test } from "vitest";
import { darwinAvailableBytes } from "../playwright-workers.mjs";

test("macOS memory comes from the kernel's available percent", () => {
  const total = 128 * 1024 ** 3;
  expect(darwinAvailableBytes("86\n", total)).toBe(Math.floor(total * 0.86));
  expect(darwinAvailableBytes(0, total)).toBe(0);
  expect(darwinAvailableBytes("100", total)).toBe(total);
  for (const bad of ["", "-5", "101", "8.5", "high", undefined])
    // @ts-expect-error undefined is deliberately outside the memory sample input contract.
    expect(darwinAvailableBytes(bad, total)).toBeUndefined();
});

test("worker planning reads current contention once without sleeping", async () => {
  const { sampleCompetingRunnableProcesses } =
    await import("../playwright-workers.mjs");
  let reads = 0;
  const result = sampleCompetingRunnableProcesses({
    readProcStat: () => {
      reads++;
      return "procs_running 4\n";
    },
  });
  expect(result).toEqual({ samples: [3], competingRunnable: 3 });
  expect(reads).toBe(1);
});
