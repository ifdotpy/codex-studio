import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import { MantineProvider, Tabs } from "@mantine/core";
import { expect, fn, userEvent, within } from "storybook/test";
import ServerManager from "./ServerManager";
import ServerDetailsEditor from "./ServerDetailsEditor";
import { StudioSettingsLayout } from "../components/StudioSettingsTabs";
import type { DiscoverySnapshot } from "./discoveryModel";
import type { ServerNavigation } from "./navigation";
import type { StudioServer } from "./registry";
import { theme } from "../theme";

const servers: StudioServer[] = [
  {
    id: "local",
    label: "Workstation",
    alias: "MAC",
    origin: "https://magritte-server.tail5d8bde.ts.net",
  },
  {
    id: "build",
    label: "Build server",
    alias: "BLD",
    origin: "https://build-release-services.tail5d8bde.ts.net",
    credentialId: "fixture",
  },
  {
    id: "travel",
    label: "Travel laptop",
    alias: "AIR",
    origin: "https://travel-macbook.tail5d8bde.ts.net",
    credentialId: "fixture",
  },
];
const navigation = (
  system: string,
  agentsRunning: number,
): ServerNavigation => ({
  ready: true,
  system,
  agentsRunning,
  projects: [],
  chats: [],
  alerts: [],
  opened: null,
  error: "",
  busy: agentsRunning > 0,
});
function ServersFixture({ light = false }: { light?: boolean }) {
  const [snapshot, setSnapshot] = useState<DiscoverySnapshot>({
    localServerId: "workstation",
    localLabel: "Workstation",
    localOrigin: servers[0].origin,
    autoPair: true,
    autoUpdate: true,
    aliases: { local: "MAC", build: "BLD", travel: "AIR" },
    servers: [],
  });
  const [value, setValue] = useState<"servers" | "accounts">("servers");
  return (
    <MantineProvider theme={theme} forceColorScheme={light ? "light" : "dark"}>
      <StudioSettingsLayout
        value={value}
        onChange={(next) =>
          setValue(next === "servers" ? "servers" : "accounts")
        }
      >
        <Tabs.Panel value="servers">
          <ServerManager
            servers={servers}
            localEnabled
            add={() => {}}
            remove={() => {}}
            statuses={{ local: "live", build: "offline", travel: "connecting" }}
            lastSeen={{
              build: Date.now() / 1000 - 7200,
              travel: Date.now() / 1000 - 600,
            }}
            navigation={{
              local: navigation("Linux", 3),
              build: navigation("Linux", 0),
              travel: navigation("Darwin", 0),
            }}
            accountsByServer={{
              local: Array.from({ length: 5 }, (_, i) => ({
                provider: i < 3 ? "codex" : "claude",
                label: `Team ${i}`,
                email: null,
                plan: "Pro",
                status: "ready",
                isDefault: i === 0,
              })),
            }}
            discovery={{
              snapshot: { ...snapshot, aliases: snapshot.aliases },
              checkedAt: Date.now() / 1000,
              busy: false,
              error: "",
              excluded: new Set<string>(),
              actions: {
                find: () => {},
                setAutoPair: () => {},
                setAutoUpdate: () => {},
              },
              setName: async (_id, label) => {
                setSnapshot((old) => ({ ...old, localLabel: label }));
                return true;
              },
              setAlias: async (id, alias) => {
                setSnapshot((old) => ({
                  ...old,
                  aliases: { ...old.aliases, [id]: alias },
                }));
                return true;
              },
              retry: undefined,
              revoke: () => {},
              allowAgain: () => {},
              addAgain: () => {},
            }}
          />
        </Tabs.Panel>
        <Tabs.Panel value="accounts">Accounts</Tabs.Panel>
      </StudioSettingsLayout>
    </MantineProvider>
  );
}
const meta = {
  title: "Settings/Servers",
  component: ServersFixture,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof ServersFixture>;
export default meta;
type Story = StoryObj<typeof meta>;
export const Dark: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(
      canvasElement.querySelector('[data-settings-server="local"]'),
    ).toHaveAttribute("data-studio-state", "working");
    await expect(
      canvasElement.querySelector('[data-settings-server="build"]'),
    ).toHaveAttribute("data-studio-state", "error");
    await expect(
      canvasElement.querySelector('[data-settings-server="travel"]'),
    ).toHaveAttribute("data-studio-state", "paused");
    await expect(canvas.getByText("Codex 3", { exact: true })).toBeVisible();
    await expect(canvas.getByText("Claude 2", { exact: true })).toBeVisible();
  },
};
export const Light: Story = {
  args: { light: true },
  globals: { colorScheme: "light" },
};
const saveName = fn(async (_value: string) => true);
const saveAlias = fn(async (_value: string) => false);
export const PartialSaveRetry: Story = {
  render: () => (
    <ServerDetailsEditor
      name="Workstation"
      alias="MAC"
      usedAliases={["BLD"]}
      saveName={saveName}
      saveAlias={saveAlias}
    />
  ),
  beforeEach: () => {
    saveName.mockClear();
    saveAlias.mockClear();
    saveAlias.mockResolvedValueOnce(false).mockResolvedValue(true);
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const summary = canvasElement.querySelector("summary");
    if (!(summary instanceof HTMLElement))
      throw new Error("Missing Edit disclosure");
    await userEvent.click(summary);
    const button = canvas.getByRole("button", { name: "Save" });
    await expect(button).toBeDisabled();
    const name = canvas.getByRole("textbox", {
      name: "Server name",
    });
    const alias = canvas.getByRole("textbox", {
      name: "Server alias",
    });
    await userEvent.clear(name);
    await userEvent.type(name, "Release workstation");
    await userEvent.clear(alias);
    await userEvent.type(alias, "REL");
    await userEvent.click(button);
    await expect(canvas.getByText("Name saved.")).toBeVisible();
    await expect(
      canvas.getByText(
        "The alias result is unknown. Check server settings or retry the saved request.",
      ),
    ).toBeVisible();
    await expect(saveName).toHaveBeenCalledTimes(1);
    await expect(saveName).toHaveBeenCalledWith("Release workstation");
    await expect(saveAlias).toHaveBeenCalledTimes(1);
    await expect(saveAlias).toHaveBeenCalledWith("REL");
    await userEvent.click(button);
    await expect(canvas.getByText("Alias saved.")).toBeVisible();
    await expect(saveName).toHaveBeenCalledTimes(1);
    await expect(saveAlias).toHaveBeenCalledTimes(2);
    await expect(button).toBeDisabled();
  },
};
