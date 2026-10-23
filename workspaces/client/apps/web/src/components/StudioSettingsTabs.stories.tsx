import type { Meta, StoryObj } from "@storybook/react-vite";
import {
  MantineProvider,
  useMantineColorScheme,
  NativeSelect,
  Tabs,
  TextInput,
} from "@mantine/core";
import { useEffect, useState } from "react";
import { expect, userEvent, within } from "storybook/test";
import {
  StudioSettingsLayout,
  studioSettingsTabs,
  type StudioSettingsTab,
} from "./StudioSettingsTabs";
import { SettingsRow, SettingsSection } from "./ui/primitives";
import { ProviderMark, accountLimitView } from "./AccountTiles";
import { AccountCapacityView } from "./Accounts";
import AccountGroup from "./AccountGroup";
import { theme } from "../theme";
import "../servers/servers.css";

function SettingsFixture({ light = false }: { light?: boolean }) {
  const { setColorScheme } = useMantineColorScheme();
  useEffect(() => {
    setColorScheme(light ? "light" : "dark");
  }, [light, setColorScheme]);
  const [value, setValue] = useState<StudioSettingsTab>("accounts");
  return (
    <MantineProvider theme={theme} forceColorScheme={light ? "light" : "dark"}>
      <div className="studio-settings-panel" data-testid="studio-settings">
        <StudioSettingsLayout
          value={value}
          onChange={(next) => {
            if (next) setValue(next as StudioSettingsTab);
          }}
        >
          {studioSettingsTabs.map(([id, title]) => (
            <Tabs.Panel key={id} value={id} pt="md">
              {id === "accounts" ? (
                <AccountStrips />
              ) : (
                <SettingsSection
                  title={title}
                  help="Preferences for this Studio workspace."
                >
                  <SettingsRow
                    label={<label htmlFor={`${id}-name`}>Hide old chats</label>}
                    help="Projects with fewer chats always show all of them."
                  >
                    <TextInput id={`${id}-name`} defaultValue="24" />
                  </SettingsRow>
                  <SettingsRow
                    label={
                      <label htmlFor={`${id}-updates`}>Provider updates</label>
                    }
                    help="Check for new provider versions when Studio starts."
                  >
                    <NativeSelect
                      id={`${id}-updates`}
                      data={["Check automatically", "Check manually"]}
                    />
                  </SettingsRow>
                </SettingsSection>
              )}
            </Tabs.Panel>
          ))}
        </StudioSettingsLayout>
      </div>
    </MantineProvider>
  );
}

function AccountStrips() {
  return (
    <SettingsSection
      title="Accounts"
      help="Provider capacity across your development projects."
    >
      <div className="accounts-grid">
        {[
          ["Codex", "Release team", 76],
          ["Claude", "Review team", 32],
          ["Codex", "Documentation", 8],
        ].map(([provider, label, remaining]) => (
          <article className="account-row" key={label}>
            <div className="account-card-heading">
              <span role="img" aria-label={String(provider)}>
                <ProviderMark provider={String(provider).toLowerCase()} />
              </span>
              <strong title={String(label)}>{label}</strong>
            </div>
            <AccountCapacityView
              account={{ status: "ready", label: String(label), email: null }}
              card
              extraDetails={
                <AccountGroup
                  account={{
                    id: String(label),
                    label: String(label),
                    provider: provider === "Claude" ? "claude" : "codex",
                    status: "ready",
                    home: "/fixture/accounts",
                    source: "fixture",
                  }}
                  onSaved={() => {}}
                />
              }
              view={accountLimitView(
                { id: String(label), status: "ready" },
                {
                  accountKey: String(label),
                  at: Date.now() / 1000,
                  data: {
                    rateLimits: {
                      limitId: String(provider).toLowerCase(),
                      primary: {
                        usedPercent: 100 - Number(remaining),
                        windowDurationMins: 300,
                        resetsAt: Date.now() / 1000 + 3600,
                      },
                    },
                  },
                },
              )}
            />
          </article>
        ))}
      </div>
    </SettingsSection>
  );
}

const meta = {
  title: "Settings/Studio navigation",
  component: SettingsFixture,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof SettingsFixture>;
export default meta;
type Story = StoryObj<typeof meta>;
export const Dark: Story = {};
export const Light: Story = { args: { light: true } };
export const SectionSelectionPreservesFields: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("tab", { name: "Appearance" }));
    const input = canvas.getByRole("textbox", { name: "Hide old chats" });
    await userEvent.clear(input);
    await userEvent.type(input, "32");
    await expect(input).toHaveValue("32");
    await expect(
      canvas.getByRole("combobox", { name: "Provider updates" }),
    ).toHaveValue("Check automatically");
    const back = canvas.queryByRole("button", { name: "Back" });
    if (back) await userEvent.click(back);
    await userEvent.click(canvas.getByRole("tab", { name: "Accounts" }));
    const summary = canvasElement.querySelector(
      ".account-limit-details > summary",
    );
    if (!(summary instanceof HTMLElement))
      throw new Error("Missing account disclosure");
    await expect(
      canvas.queryByRole("textbox", { name: "Group of Release team" }),
    ).not.toBeVisible();
    for (const row of canvasElement.querySelectorAll(".account-row")) {
      await expect(row.querySelector(".account-window-grid")).toBeVisible();
    }
    await userEvent.click(summary);
    await expect(
      canvas.getByRole("textbox", { name: "Group of Release team" }),
    ).toBeVisible();
    const meters = canvas.getAllByRole("meter", { name: "Remaining limit" });
    await expect(
      meters.map((meter) => meter.getAttribute("aria-valuenow")),
    ).toEqual(["76", "32", "8"]);
  },
};
