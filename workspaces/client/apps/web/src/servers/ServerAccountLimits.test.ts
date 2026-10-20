import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import type { Account } from "../components/Accounts";
import ServerAccountLimits, { matchServerAccount } from "./ServerAccountLimits";

const account = (fields: object) =>
  ({
    id: "k",
    label: "Work",
    provider: "codex",
    email: "Person@Example.test",
    status: "ready",
    ...fields,
  }) as unknown as Account;

test("a server account shows its stored turn start limits in the collapsed summary", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(ServerAccountLimits, {
        account: account({
          startThrottle: { burst: 8, pauseSeconds: 30, maxRunningTurns: 16 },
        }),
        onSaved: () => {},
      }),
    ),
  );
  expect(markup).toContain(
    "Turn start limits: 8 per burst · 30 s pause · 16 running",
  );
  expect(markup).toContain("<details");
  expect(markup).not.toContain("<details open");
});

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
