import { Tabs } from "@mantine/core";
export const studioSettingsTabs = [
  ["accounts", "Accounts"],
  ["appearance", "Appearance"],
  ["federation", "Federation"],
  ["servers", "Servers"],
  ["linux-vm", "Linux VM"],
  ["hotkeys", "Hotkeys"],
] as const;
export type StudioSettingsTab = (typeof studioSettingsTabs)[number][0];
export function isStudioSettingsTab(
  value: unknown,
): value is StudioSettingsTab {
  return studioSettingsTabs.some(([id]) => id === value);
}
export default function StudioSettingsTabs({
  serversOnly = false,
}: {
  serversOnly?: boolean;
}) {
  return (
    <Tabs.List aria-label="Studio settings">
      {studioSettingsTabs.map(([id, label]) => (
        <Tabs.Tab
          key={id}
          value={id}
          disabled={serversOnly && id !== "servers"}
        >
          {label}
        </Tabs.Tab>
      ))}
    </Tabs.List>
  );
}
