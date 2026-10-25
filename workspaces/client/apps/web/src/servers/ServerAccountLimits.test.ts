import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { Accordion, MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import { accountLimitView } from "../components/AccountTiles";
import { AccountCapacityView, type Account } from "../components/Accounts";
import type { AccountLimitsSnapshot } from "../usage/accountUsage";
import {
  AccountLimitsAcrossServers,
  matchServerAccount,
  newestReading,
  readingAge,
  ServerTurnStartLimits,
} from "./ServerAccountLimits";

const account = (fields: object) =>
  ({
    id: "k",
    label: "Work",
    provider: "codex",
    email: "Person@Example.test",
    status: "ready",
    ...fields,
  }) as unknown as Account;
const limits = (accountKey: string, at: number, usedPercent: number) =>
  ({
    accountKey,
    at,
    data: { accountId: null, rateLimits: { primary: { usedPercent } } },
    error: null,
  }) as unknown as AccountLimitsSnapshot;
const render = (element: ReturnType<typeof createElement>) =>
  renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(
        Accordion,
        { keepMounted: false },
        createElement(Accordion.Item, { value: "account" }, element),
      ),
    ),
  );
const count = (text: string, part: string) => text.split(part).length - 1;

test.each(["codex", "claude"] as const)(
  "%s card turn start limits name the Studio server that stores them",
  (provider) => {
    const fullAccount = account({
      provider,
      startThrottle: { burst: 8, pauseSeconds: 30, maxRunningTurns: 16 },
    });
    const matched = matchServerAccount([fullAccount], {
      provider,
      email: "person@example.test",
      label: "Work",
    });
    expect(matched).toBe(fullAccount);
    const markup = render(
      createElement(ServerTurnStartLimits, {
        source: {
          serverId: "mbp",
          serverLabel: "Igor MBP",
          account: matched!,
        },
        onSaved: () => {},
      }),
    );
    expect(markup).toContain(
      "Turn start limits on Igor MBP: 8 per burst · 30 s pause · 16 running",
    );
    expect(markup).toContain('aria-expanded="false"');
    expect(markup).not.toContain("Save turn start limits");
  },
);

test("a panel row maps to its server's account by provider and email, or label without an email", () => {
  const codex = account({});
  const claude = account({ provider: "claude", id: "c" });
  const unnamed = account({ email: null, label: "Local" });
  const list = [claude, codex, unnamed];
  const row = {
    provider: "codex" as const,
    email: "person@example.test",
    label: "x",
  };
  expect(matchServerAccount(list, row)).toBe(codex);
  expect(
    matchServerAccount(list, { ...row, email: null, label: "Local" }),
  ).toBe(unnamed);
  expect(matchServerAccount(undefined, row)).toBeUndefined();
});

test("a full account match honors the exact server account key without a label fallback", () => {
  const first = account({ id: "first", provider: "claude" });
  const second = account({ id: "second", provider: "claude" });
  const row = {
    accountKey: "second",
    provider: "claude" as const,
    email: "person@example.test",
    label: "Work",
  };
  expect(matchServerAccount([first, second], row)).toBe(second);
  expect(matchServerAccount([first], row)).toBeUndefined();
  const { accountKey: _key, ...legacy } = row;
  expect(matchServerAccount([first, second], legacy)).toBeUndefined();
});

test("the account card uses the newest reading with data from any server", () => {
  const older = { server: "studio", limits: limits("a", 100, 10) };
  const newer = { server: "mbp", limits: limits("b", 200, 67) };
  const reading = { server: "idle", limits: null };
  const failed = {
    server: "down",
    limits: { accountKey: "c", at: null, data: null, error: "Offline" },
  };
  expect(newestReading([older, newer, reading])).toBe(newer);
  expect(newestReading([newer, older])).toBe(newer);
  // Without data, a read in progress comes before a read error.
  expect(newestReading([failed, reading])).toBe(reading);
  expect(newestReading([failed])).toBe(failed);
  // A finished read without data is shown, not taken as a read in progress.
  const empty = {
    server: "empty",
    limits: { accountKey: "d", at: null, data: null, error: null },
  };
  expect(newestReading([empty])).toBe(empty);
  expect(newestReading([empty, failed])).toBe(failed);
  expect(newestReading([])).toBeUndefined();
});

test("a reading age reads in plain units", () => {
  expect(readingAge(5)).toBe("just now");
  expect(readingAge(5 * 60 + 10)).toBe("5 min ago");
  expect(readingAge(3 * 3600)).toBe("3 h ago");
  expect(readingAge(2 * 86400)).toBe("2 d ago");
});

test("one reading shows one remaining limit with its age and server", () => {
  const owner = account({ id: "b" });
  const markup = render(
    createElement(AccountCapacityView, {
      card: true,
      account: owner,
      view: accountLimitView(owner, limits("b", 200, 67), 300),
      note: "Updated 1 min ago on Igor MBP",
    }),
  );
  expect(count(markup, "Remaining limit</span>")).toBe(1);
  expect(markup).toContain("33% left");
  expect(markup).not.toContain("Updated 1 min ago on Igor MBP");
  expect(markup).not.toContain("account-window-meter");
});

test("an account on two servers shows one limit, not one per server", () => {
  const markup = render(
    createElement(AccountLimitsAcrossServers, {
      sources: [
        {
          serverId: "studio",
          serverLabel: "Studio server",
          account: account({ id: "a" }),
        },
        {
          serverId: "mbp",
          serverLabel: "Igor MBP",
          account: account({ id: "b" }),
        },
      ],
    }),
  );
  expect(count(markup, "Remaining limit</span>")).toBe(1);
  expect(markup).not.toContain("Studio server");
  expect(markup).not.toContain("Igor MBP");
});

test("an account signed out on every server shows its limit as unavailable", () => {
  const markup = render(
    createElement(AccountLimitsAcrossServers, {
      sources: [
        {
          serverId: "studio",
          serverLabel: "Studio server",
          account: account({ status: "signedOut" }),
        },
      ],
    }),
  );
  expect(markup).toContain("Unavailable");
  expect(markup).not.toContain("Reading limits");
});
