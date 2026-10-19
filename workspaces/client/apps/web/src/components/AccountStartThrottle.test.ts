import { expect, test } from "vitest";
import { THROTTLE_FIELDS, accountThrottle } from "./AccountStartThrottle";
import type { Account } from "./Accounts";
import * as shared from "../generated/sharedValues";

const account = (provider: "codex" | "claude", startThrottle?: object) =>
  ({ id: provider, provider, startThrottle }) as unknown as Account;

test("an account without settings shows its provider's generated defaults", () => {
  expect(accountThrottle(account("codex"))).toEqual({
    burst: shared.ACCOUNT_START_BURST_DEFAULT_CODEX,
    pauseSeconds: shared.ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CODEX,
    maxRunningTurns: shared.ACCOUNT_RUNNING_TURNS_DEFAULT_CODEX,
  });
  expect(accountThrottle(account("claude"))).toEqual({
    burst: shared.ACCOUNT_START_BURST_DEFAULT_CLAUDE,
    pauseSeconds: shared.ACCOUNT_START_PAUSE_DEFAULT_SECONDS_CLAUDE,
    maxRunningTurns: shared.ACCOUNT_RUNNING_TURNS_DEFAULT_CLAUDE,
  });
  const stored = { burst: 2, pauseSeconds: 5, maxRunningTurns: 3 };
  expect(accountThrottle(account("codex", stored))).toEqual(stored);
});

test("each field uses the generated bounds and says Studio limits turn starts only", () => {
  expect(
    THROTTLE_FIELDS.map((spec) => [spec.field, spec.min, spec.max]),
  ).toEqual([
    ["burst", shared.ACCOUNT_START_BURST_MIN, shared.ACCOUNT_START_BURST_MAX],
    [
      "pauseSeconds",
      shared.ACCOUNT_START_PAUSE_MIN_SECONDS,
      shared.ACCOUNT_START_PAUSE_MAX_SECONDS,
    ],
    [
      "maxRunningTurns",
      shared.ACCOUNT_RUNNING_TURNS_MIN,
      shared.ACCOUNT_RUNNING_TURNS_MAX,
    ],
  ]);
  for (const spec of THROTTLE_FIELDS) {
    expect(spec.tooltip).toContain("Studio counts only turn starts");
    expect(spec.tooltip).toContain(
      `Codex ${shared.ACCOUNT_START_BURST_DEFAULT_CODEX} starts per burst`,
    );
  }
});
