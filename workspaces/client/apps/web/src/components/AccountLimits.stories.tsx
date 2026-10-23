import { expect, userEvent, waitFor, within } from "storybook/test";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { useEffect, useMemo } from "react";
import { MantineProvider } from "@mantine/core";
import Usage from "./Usage";
import { AccountCapacityView } from "./Accounts";
import { accountLimitView, ProviderMark } from "./AccountTiles";
import { createServerServices } from "../servers/serverServices";
import { ServerServicesProvider } from "../servers/ServerServicesContext";
import type { Agent } from "../types";
import { accountLimits } from "../usage/accountUsage";
import { theme } from "../theme";

import { accountLimitFixtures } from "./AccountLimits.fixtures";

function AccountLimitsFixture({
  settings = false,
  light = false,
}: {
  settings?: boolean;
  light?: boolean;
}) {
  const services = useMemo(() => {
    const value = createServerServices({
      id: "local",
      label: "Fixture",
      origin: globalThis.window.location.origin,
    });
    value.watchResourceReads = (_refs, read) => {
      void read();
      return Object.assign(() => {}, {
        refresh: () => {
          void read();
        },
      });
    };
    return value;
  }, []);
  useEffect(() => () => services.close(), [services]);
  return (
    <MantineProvider theme={theme} forceColorScheme={light ? "light" : "dark"}>
      <ServerServicesProvider services={services}>
        {settings ? (
          <div
            className="accounts-grid"
            style={{ maxWidth: "var(--studio-form-max)" }}
          >
            {accountLimitFixtures.map((item) => (
              <article className="account-row" key={item.key}>
                <div className="account-card-heading">
                  <span
                    role="img"
                    aria-label={item.provider === "claude" ? "Claude" : "Codex"}
                  >
                    <ProviderMark provider={item.provider || "codex"} />
                  </span>
                  <strong>{item.label}</strong>
                  <span className="account-plan">
                    {item.provider === "claude" ? "Max" : "Pro"}
                  </span>
                </div>
                <AccountCapacityView
                  card
                  account={{
                    status: "ready",
                    label: item.label,
                    email: item.email,
                  }}
                  view={accountLimitView(
                    {
                      id: item.key,
                      accountId: item.accountId,
                      status: "ready",
                    },
                    accountLimits(item.limits, item.key, item.accountId),
                  )}
                  extraDetails={<small>{item.email}</small>}
                />
              </article>
            ))}
          </div>
        ) : (
          <div
            style={{
              display: "flex",
              alignItems: "end",
              containerName: "conversation-footer",
              containerType: "inline-size",
              height: "calc(100dvh - var(--mantine-spacing-xl))",
            }}
          >
            <Usage
              agent={
                {
                  id: "fixture",
                  accountKey: "codex-main",
                  provider: "codex",
                  contextUsage: { tokens: 12000, window: 128000 },
                } as Agent
              }
              stateDir="limits-fixture"
              limits={accountLimitFixtures[0].limits}
              accounts={accountLimitFixtures}
              reload={() => {}}
              opened
              onChange={() => {}}
            />
          </div>
        )}
      </ServerServicesProvider>
    </MantineProvider>
  );
}
const meta = {
  title: "Settings/Account limits",
  component: AccountLimitsFixture,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    const original = globalThis.window.fetch;
    globalThis.window.fetch = async (input, init) => {
      const request = new Request(input, init),
        url = new URL(request.url);
      if (["/api/costs", "/api/session-cost"].includes(url.pathname))
        return new Response(
          JSON.stringify(
            url.pathname === "/api/costs"
              ? {
                  accountKey: url.searchParams.get("account_key"),
                  data: { todayUSD: 4.2, last30DaysUSD: 28.35 },
                }
              : { pricingState: "ready", totalUSD: 1.25 },
          ),
          { headers: { "content-type": "application/json" } },
        );
      return original(input, init);
    };
    return () => {
      globalThis.window.fetch = original;
    };
  },
} satisfies Meta<typeof AccountLimitsFixture>;
export default meta;
type Story = StoryObj<typeof meta>;
export const PopoverDark: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement.ownerDocument.body);
    const popover = canvasElement.ownerDocument.querySelector(
      ".account-limits-popover",
    );
    if (!(popover instanceof HTMLElement))
      throw new Error("Missing limits popover");
    await waitFor(() => expect(canvas.getByText("$4.20")).toBeVisible());
    await waitFor(() => expect(getComputedStyle(popover).opacity).toBe("1"));
    await new Promise<void>((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
    );
    for (const element of [popover, ...popover.querySelectorAll("*")]) {
      const overflow = getComputedStyle(element).overflowY;
      if (overflow === "auto" || overflow === "scroll")
        await expect(element.scrollHeight).toBeLessThanOrEqual(
          element.clientHeight + 1,
        );
    }
    const selectedName = () =>
      canvasElement.ownerDocument.querySelector(
        ".account-limits-detail-panel[data-active] .account-limits-selected-name strong",
      );
    await expect(selectedName()).toHaveTextContent("main@example.test");
    await expect(selectedName()).toHaveAttribute("title", "main@example.test");
    const original = popover.getBoundingClientRect();
    await expect(canvas.getAllByRole("tab")).toHaveLength(3);
    await userEvent.click(
      canvas.getByText("Claude", { selector: ".setup-provider" }),
    );
    await expect(canvas.getAllByRole("tab")).toHaveLength(2);
    await expect(selectedName()).toHaveTextContent("Claude code 2");
    await expect(
      canvasElement.ownerDocument.querySelector(
        ".account-limits-detail-panel[data-active] .account-plan",
      ),
    ).toHaveTextContent("max");
    await expect(popover.getBoundingClientRect().height).toBe(original.height);
    await expect(popover.getBoundingClientRect().y).toBe(original.y);
    await expect(
      canvas.getByRole("link", { name: "Open free resets" }),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByText("Codex", { selector: ".setup-provider" }),
    );
    await expect(popover.getBoundingClientRect().height).toBe(original.height);
    await expect(
      canvas.getByRole("button", { name: "Apply reset" }),
    ).toBeVisible();
  },
};
export const PopoverLight: Story = {
  args: { light: true },
  globals: { colorScheme: "light" },
  play: PopoverDark.play,
};
export const SettingsDark: Story = {
  args: { settings: true },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(canvas.getAllByRole("article")).toHaveLength(5);
    await expect(
      canvas.getByRole("meter", { name: "Spark 5h remaining" }),
    ).toBeVisible();
    await expect(
      canvas.getByRole("meter", { name: "Opus 7d remaining" }),
    ).toBeVisible();
    await expect(
      canvas.getByText("Limits temporarily unavailable", {
        selector: ".account-window-grid > small",
      }),
    ).toBeVisible();
    const rows = [...canvasElement.querySelectorAll(".account-row")];
    for (const [index, row] of rows.entries()) {
      const windows = row.querySelector(".account-window-grid");
      await expect(windows).toBeVisible();
      if (index > 0)
        await expect(
          row.getBoundingClientRect().top -
            rows[index - 1].getBoundingClientRect().bottom,
        ).toBeGreaterThanOrEqual(16);
    }
  },
};
export const SettingsLight: Story = {
  args: { settings: true, light: true },
  globals: { colorScheme: "light" },
  play: SettingsDark.play,
};
