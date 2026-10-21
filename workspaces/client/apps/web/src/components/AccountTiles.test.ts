import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { expect, test } from "vitest";
import type { AccountLimitsSnapshot } from "../usage/accountUsage";
import type { Account } from "./Accounts";
import { accountLimitView, AccountTileLabel } from "./AccountTiles";

const reading = (fields: object = {}) =>
  ({
    accountKey: "work",
    at: 100,
    data: { accountId: null, rateLimits: { primary: { usedPercent: 67 } } },
    error: null,
    ...fields,
  }) as unknown as AccountLimitsSnapshot;
const account = (status: Account["status"] = "ready") => ({
  id: "work",
  accountId: null,
  status,
});

test("an account picker tile states the remaining limit next to the name", () => {
  const markup = renderToStaticMarkup(
    createElement(AccountTileLabel, { name: "Work", remaining: 33 }),
  );
  expect(markup).toContain("Work");
  expect(markup).toContain("33% left");
  expect(
    renderToStaticMarkup(
      createElement(AccountTileLabel, { name: "Work", remaining: null }),
    ),
  ).not.toContain("left");
});

test("the remaining limit is known only for a signed-in account with data", () => {
  expect(accountLimitView(account(), reading(), 200).remaining).toBe(33);
  expect(accountLimitView(account("signedOut"), reading(), 200).remaining).toBe(
    null,
  );
  const failed = accountLimitView(
    account(),
    reading({ data: null, error: "Offline" }),
    200,
  );
  expect(failed.remaining).toBeNull();
  expect(failed.error).toBe("Offline");
  // A reading of another account never shows.
  expect(
    accountLimitView(account(), reading({ accountKey: "other" }), 200)
      .remaining,
  ).toBeNull();
  expect(accountLimitView(account(), null, 200).reading).toBe(true);
  // A picker still marks a signed-out account whose limit is used up.
  expect(
    accountLimitView(account("signedOut"), reading(), 200, false).remaining,
  ).toBe(33);
});
