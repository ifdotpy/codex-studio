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
import { AccountCapacityView } from "./Accounts";
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
              <strong>{label}</strong>
            </div>
            <span className="account-email">{provider}</span>
            <AccountCapacityView
              account={{ status: "ready", label: String(label), email: null }}
              card
              view={{
                limits: null,
                buckets: [],
                remaining: Number(remaining),
                error: null,
                reading: false,
              }}
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
    const meters = canvas.getAllByRole("meter", { name: "Remaining limit" });
    await expect(
      meters.map((meter) => meter.getAttribute("aria-valuenow")),
    ).toEqual(["76", "32", "8"]);
  },
};
