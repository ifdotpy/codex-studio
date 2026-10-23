import { Button, Tabs } from "@mantine/core";
import { ArrowLeft } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import "./studio-settings.css";
export const studioSettingsTabs = [
  ["accounts", "Accounts"],
  ["appearance", "Appearance"],
  ["agents", "Agents"],
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
  onSelect,
}: {
  serversOnly?: boolean;
  onSelect?: () => void;
}) {
  return (
    <Tabs.List aria-label="Studio settings">
      {studioSettingsTabs.map(([id, label]) => (
        <Tabs.Tab
          key={id}
          value={id}
          disabled={serversOnly && id !== "servers"}
          onClick={onSelect}
        >
          {label}
        </Tabs.Tab>
      ))}
    </Tabs.List>
  );
}

/** One section navigation shared by the local and multi-server settings. */
export function StudioSettingsLayout({
  value,
  onChange,
  children,
}: {
  value: StudioSettingsTab;
  onChange: (value: string | null) => void;
  children: ReactNode;
}) {
  const [detail, setDetail] = useState(value !== "accounts");
  const heading = useRef<HTMLHeadingElement>(null);
  const navigation = useRef<HTMLDivElement>(null);
  const title = studioSettingsTabs.find(([id]) => id === value)?.[1];
  useEffect(() => {
    if (detail && heading.current?.getClientRects().length)
      heading.current.focus({ preventScroll: true });
  }, [detail, value]);
  return (
    <Tabs
      value={value}
      onChange={onChange}
      orientation="vertical"
      variant="pills"
      className="studio-settings-tabs"
      data-detail={detail || undefined}
    >
      <div className="studio-settings-navigation" ref={navigation}>
        <StudioSettingsTabs onSelect={() => setDetail(true)} />
      </div>
      <div className="studio-settings-content">
        <header className="studio-settings-detail-heading">
          <Button
            variant="subtle"
            leftSection={<ArrowLeft aria-hidden="true" />}
            onClick={() => {
              setDetail(false);
              requestAnimationFrame(() =>
                navigation.current
                  ?.querySelector<HTMLButtonElement>('[data-active="true"]')
                  ?.focus({ preventScroll: true }),
              );
            }}
          >
            Back
          </Button>
          <h2 ref={heading} tabIndex={-1}>
            {title}
          </h2>
        </header>
        {children}
      </div>
    </Tabs>
  );
}
