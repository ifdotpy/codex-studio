import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import { AccountTransferChip } from "./Accounts";

const render = (
  transfer: Parameters<typeof AccountTransferChip>[0]["transfer"],
) =>
  renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(AccountTransferChip, { transfer }),
    ),
  );

test("shows one short line for a blocked team move and keeps the reason for the tooltip", () => {
  const markup = render({
    id: "t-1",
    status: "pending",
    total: 2,
    completed: 1,
    nativeHistoryPending: 1,
    blocked: [
      {
        id: "lead",
        name: "Codex Studio Attar",
        reason: "This profile's account changed.",
      },
    ],
  });
  expect(markup).toContain("Account move blocked");
  expect(markup).toContain("data-blocked");
  expect(markup).toContain(
    "Codex Studio Attar: This profile&#x27;s account changed.",
  );
  // The long text of the old status block is not in the composer row.
  expect(markup).not.toContain("History transfer in progress");
});

test("shows the remaining count while a move runs and nothing after it ends", () => {
  expect(
    render({ id: "t-2", status: "pending", total: 3, completed: 1 }),
  ).toContain("Moving account · 2 left");
  expect(
    render({ id: "t-3", status: "completed", total: 3, completed: 3 }),
  ).not.toContain("account-transfer-chip");
  expect(render(undefined)).not.toContain("account-transfer-chip");
});
