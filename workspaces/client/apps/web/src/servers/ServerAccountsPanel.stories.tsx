import type { Meta, StoryObj } from "@storybook/react-vite";
import { useEffect, useMemo } from "react";
import { expect, userEvent, within, waitFor } from "storybook/test";
import ServerAccountsPanel from "./ServerAccountsPanel";
import { createServerServices } from "./serverServices";
import { ServerServicesProvider } from "./ServerServicesContext";
import type { ServerAccount } from "./navigation";
import type { Account } from "../components/Accounts";
import "./servers.css";

const accounts = [
  {
    id: "release",
    label: "Release team",
    email: "release@example.test",
    provider: "codex",
    plan: "Pro",
    status: "ready",
    isDefault: true,
    home: "/fixture/release",
    source: "fixture",
  },
  {
    id: "review",
    label: "Review team",
    email: "review@example.test",
    provider: "claude",
    plan: "Max",
    status: "signedOut",
    isDefault: false,
    home: "/fixture/review",
    source: "fixture",
  },
] satisfies (Account & { isDefault: boolean })[];

function ServerAccountsFixture() {
  const services = useMemo(() => {
    const value = createServerServices({
      id: "local",
      label: "Workstation",
      origin: window.location.origin,
    });
    value.resources.watchResourceChanges = (_resource, changed) => {
      changed();
      return () => {};
    };
    value.resources.watchResourceConnection = () => () => {};
    return value;
  }, []);
  useEffect(() => () => services.close(), [services]);
  return (
    <div style={{ maxWidth: "var(--studio-form-max)" }}>
      <ServerServicesProvider services={services}>
        <ServerAccountsPanel
          servers={[
            {
              id: "local",
              label: "Workstation",
              origin: window.location.origin,
            },
            {
              id: "remote",
              label: "Build server",
              origin: window.location.origin,
            },
          ]}
          accountsByServer={{ local: accounts satisfies ServerAccount[] }}
          statuses={{ local: "ready", remote: "offline" }}
          startAccount={() => {}}
          accountAction={() => {}}
        />
      </ServerServicesProvider>
    </div>
  );
}

const meta = {
  title: "Settings/Server account strips",
  component: ServerAccountsFixture,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    const original = window.fetch;
    window.fetch = async (input, init) => {
      const request = new Request(input, init);
      const url = new URL(request.url);
      const json = (data: unknown) =>
        new Response(JSON.stringify(data), {
          headers: { "content-type": "application/json" },
        });
      if (url.pathname === "/api/accounts") return json({ accounts });
      if (url.pathname === "/api/limits")
        return json({
          accountKey: url.searchParams.get("account_key"),
          at: Date.now() / 1000,
          data: {
            rateLimits: { limitId: "codex", primary: { usedPercent: 24 } },
          },
        });
      return original(input, init);
    };
    return () => {
      window.fetch = original;
    };
  },
} satisfies Meta<typeof ServerAccountsFixture>;
export default meta;
type Story = StoryObj<typeof meta>;
export const WarningAndActions: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const row = canvasElement.querySelector(
      '[data-account-identity="codex:release@example.test"]',
    );
    if (!(row instanceof HTMLElement))
      throw new Error("Missing release account");
    const account = within(row);
    await expect(account.getByText("Offline", { exact: true })).toBeVisible();
    await expect(account.getByText("Default", { exact: false })).toBeVisible();
    await expect(
      canvas.getByText("Sign in needed", { exact: true }),
    ).toBeVisible();
    await expect(row.getBoundingClientRect().height).toBeLessThanOrEqual(56);
    await waitFor(() =>
      expect(
        account.getByRole("meter", { name: "Remaining limit" }),
      ).toHaveAttribute("aria-valuenow", "76"),
    );
    const details = row.querySelector(".account-limit-details > summary");
    if (!(details instanceof HTMLElement))
      throw new Error("Missing limit details");
    await userEvent.click(details);
    await expect(
      await account.findByRole("textbox", {
        name: "Group of release@example.test on Workstation",
      }),
    ).toBeVisible();
    await expect(
      account.getByRole("button", { name: "✓ Workstation" }),
    ).toBeVisible();
    await expect(
      account.getByRole("button", { name: "Build server · offline" }),
    ).toBeDisabled();
  },
};
