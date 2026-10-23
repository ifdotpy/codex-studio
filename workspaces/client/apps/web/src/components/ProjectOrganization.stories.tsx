import type { Meta, StoryObj } from "@storybook/react-vite";
import { MantineProvider, useMantineColorScheme } from "@mantine/core";
import { useEffect } from "react";
import { expect, userEvent, within } from "storybook/test";
import {
  MoveChatForm,
  ProjectNameForm,
  type Project,
} from "./ProjectOrganization";
import ProjectLocationsSettings from "./ProjectLocationsSettings";
import { SettingsSection } from "./ui/primitives";
import { theme } from "../theme";
import "./project-settings.css";

const project = {
  id: "atlas",
  name: "Atlas API",
  path: "/workspace/atlas/services/api",
  folders: [{ id: "review", name: "Release review" }],
  locations: [
    {
      serverId: "local",
      projectId: "atlas",
      path: "/workspace/atlas/services/api",
    },
    {
      serverId: "build",
      projectId: "atlas-build",
      path: "/workspace/atlas-release/services/api",
    },
  ],
} satisfies Project;

function ProjectFixture({ light = false }: { light?: boolean }) {
  const { setColorScheme } = useMantineColorScheme();
  useEffect(() => {
    setColorScheme(light ? "light" : "dark");
  }, [light, setColorScheme]);
  return (
    <MantineProvider theme={theme} forceColorScheme={light ? "light" : "dark"}>
      <div className="project-settings">
        <SettingsSection title="Project organization">
          <ProjectNameForm project={project} saved={async () => {}} />
          <MoveChatForm
            project={project}
            agent={{ id: "lead", name: "Release lead" }}
            saved={async () => {}}
          />
        </SettingsSection>
        <SettingsSection
          title="Project folders"
          help="The same project can have a working folder on each server."
        >
          <ProjectLocationsSettings
            project={project}
            servers={[
              { id: "local", label: "Workstation" },
              { id: "build", label: "Build server" },
            ]}
            saved={async () => {}}
          />
        </SettingsSection>
      </div>
    </MantineProvider>
  );
}

const meta = {
  title: "Settings/Project organization",
  component: ProjectFixture,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    const original = window.fetch;
    window.fetch = async (input, init) => {
      const request = new Request(input, init);
      if (new URL(request.url).pathname === "/api/project-locations")
        return new Response(JSON.stringify({ gitHead: "ui/redesign" }), {
          headers: { "content-type": "application/json" },
        });
      return original(input, init);
    };
    return () => {
      window.fetch = original;
    };
  },
} satisfies Meta<typeof ProjectFixture>;
export default meta;
type Story = StoryObj<typeof meta>;
export const Dark: Story = {};
export const Light: Story = { args: { light: true } };
export const AccessibleFields: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(
      canvas.getByRole("textbox", { name: "Project name" }),
    ).toHaveValue("Atlas API");
    await userEvent.selectOptions(
      canvas.getByRole("combobox", { name: "Move to folder" }),
      "review",
    );
    await expect(
      canvas.getByRole("combobox", { name: "Move to folder" }),
    ).toHaveValue("review");
    await expect(
      canvas.getByRole("button", {
        name: "Options for folder /workspace/atlas/services/api",
      }),
    ).toBeVisible();
  },
};
