import { expect, userEvent, waitFor, within } from "storybook/test";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { useEffect, useMemo } from "react";
import { Accordion, MantineProvider } from "@mantine/core";
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
  const [firstAccountFixture] = accountLimitFixtures;
  if (!firstAccountFixture) throw new Error("Missing account limit fixture");
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
          <Accordion
            keepMounted={false}
            transitionDuration={0}
            chevronPosition="left"
            className="accounts-grid"
            style={{ maxWidth: "var(--studio-form-max)" }}
          >
            {accountLimitFixtures.map((item) => (
              <Accordion.Item
                className="account-row"
                key={item.key}
                value={item.key}
              >
                <div className="account-card-heading">
                  <Accordion.Control
                    aria-label={`Limit details for ${item.email || item.label}`}
                  >
                    <span className="account-identity">
                      <span
                        role="img"
                        aria-label={
                          item.provider === "claude" ? "Claude" : "Codex"
                        }
                      >
                        <ProviderMark provider={item.provider || "codex"} />
                      </span>
                      <strong>{item.label}</strong>
                    </span>
                  </Accordion.Control>
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
              </Accordion.Item>
            ))}
          </Accordion>
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
              limits={firstAccountFixture.limits}
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
export const PopoverDark = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement.ownerDocument.body);
    const popover = canvasElement.ownerDocument.querySelector(
      ".account-limits-popover",
    );
    if (!(popover instanceof HTMLElement))
      throw new Error("Missing limits popover");
    await waitFor(() => expect(canvas.getByText("$4.20")).toBeVisible());
    await waitFor(() => expect(getComputedStyle(popover).opacity).toBe("1"));
    await canvasElement.ownerDocument.fonts.ready;
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
    const details = within(popover);
    await expect(
      details.queryByText(/Session estimate/),
    ).not.toBeInTheDocument();
    await expect(details.getByText("Estimated API cost")).toBeVisible();
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
} satisfies Story;
export const PopoverLight: Story = {
  args: { light: true },
  globals: { colorScheme: "light" },
  play: PopoverDark.play,
};
export const SettingsDark = {
  args: { settings: true },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const rows = [...canvasElement.querySelectorAll(".account-row")];
    await expect(rows).toHaveLength(5);
    for (const row of rows) {
      await expect(row.getBoundingClientRect().height).toBeLessThanOrEqual(80);
      await expect(
        row.querySelector(".account-expanded"),
      ).not.toBeInTheDocument();
    }
    const controls = canvas.getAllByRole("button", {
      name: /Limit details for/,
    });
    const first = controls[0],
      fourth = controls[3],
      third = controls[2];
    if (!first || !fourth || !third)
      throw new Error("Missing limit detail controls");
    await userEvent.click(first);
    await expect(
      canvas.getByRole("meter", { name: "Spark 5h remaining" }),
    ).toBeVisible();
    await expect(canvas.getByText("1 credit available")).toBeVisible();
    await expect(
      canvas.queryByRole("button", { name: "Apply reset" }),
    ).not.toBeInTheDocument();
    await userEvent.click(fourth);
    await expect(
      canvas.getByRole("meter", { name: "Opus 7d remaining" }),
    ).toBeVisible();
    await expect(controls[0]).toHaveAttribute("aria-expanded", "false");
    await expect(
      canvasElement.querySelectorAll(".account-expanded"),
    ).toHaveLength(1);
    await userEvent.click(third);
    await expect(
      canvas.getByText("Limits temporarily unavailable"),
    ).toBeVisible();
    await expect(
      canvasElement.querySelectorAll(".account-expanded"),
    ).toHaveLength(1);
  },
} satisfies Story;
export const SettingsLight: Story = {
  args: { settings: true, light: true },
  globals: { colorScheme: "light" },
  play: SettingsDark.play,
};
