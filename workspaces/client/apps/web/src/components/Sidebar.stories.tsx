import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { MantineProvider } from "@mantine/core";
import { expect, fn, userEvent, within } from "storybook/test";
import Sidebar, { type SidebarProps } from "./Sidebar";
import { theme } from "../theme";
import type { Agent, Snapshot } from "../types";
import { chatIndicators } from "./chat-status/chatStatusModel";
import { createSidebarServices, type SidebarBackend } from "./sidebar/services";
import "./sidebar/FleetFixtures.css";

const now = Date.now() / 1000;
const projects = ["codex-studio", "atlas-api", "orchard-web"];
const titles = [
  "Review deployment permissions",
  "Repair failing database migration",
  "Обновление интерфейса нескольких параллельных проектов",
  "Document the release checklist and rollback procedure",
  "Investigate mobile navigation",
  "Ready for the next assignment",
];
const threads = projects.flatMap((project, projectIndex) =>
  titles.map(
    (name, index) =>
      ({
        id: `${project}-${index}`,
        name,
        cwd: `/work/${project}`,
        source: "managed",
        isLead: true,
        provider: index % 2 ? "claude" : "codex",
        model: index % 2 ? "claude-opus-4-6" : "gpt-5.4",
        status: [
          "approval",
          "failed",
          "running",
          "completed",
          "paused",
          "completed",
        ][index],
        autoWake: index !== 4,
        inFlight: index === 2,
        created: now - 86400,
        updated: now - (projectIndex + index + 1) * 180,
        pinned: projectIndex === 0 && index === 2,
        threadId: `${project}-${index}-thread`,
        lastCompletedTurn: index === 3 ? "result" : null,
        lastCompletedTurnStatus: index === 3 ? "completed" : null,
        readStateSupported: true,
        ...(index === 2
          ? {
              commandProgress: [
                {
                  monitorId: `${project}-checks`,
                  stage: "Running focused browser checks",
                  startedAt: now - 46,
                  lastOutputLine: "Checking responsive navigation",
                },
              ],
            }
          : {}),
      }) as Agent,
  ),
);
const data = {
  token: "storybook-fleet",
  stateDir: "storybook-fleet",
  threads,
  chats: [],
  nodes: threads,
  edges: [],
  runtime: {
    agents: threads,
    projects: projects.map((name) => ({
      id: name,
      name,
      path: `/work/${name}`,
      created: now - 86400,
    })),
    requests: threads
      .filter((_, index) => index % titles.length === 0)
      .map((agent) => ({
        id: `request-${agent.id}`,
        agent: agent.id,
        status: "pending",
      })),
    rooms: [],
    tasks: [],
    monitors: [],
    complaints: [],
    rules: [],
    peerTeams: [],
    events: [],
    work: [],
    peerTeamsVersion: 1,
    projectOrganizationVersion: 1,
  },
} as unknown as Snapshot;
const values = new Map<string, string>();
const backend: SidebarBackend = {
  ownerId: "local",
  post: async () => {
    throw new Error("Story fixture does not write to a server.");
  },
  saved: (key, fallback) => JSON.parse(values.get(key) || "null") ?? fallback,
  save: (key, value) => {
    values.set(key, JSON.stringify(value));
  },
  storage: {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
  },
  editPreference: (key, _before, after) => {
    values.set(key, JSON.stringify(after));
    return after;
  },
  subscribePreference: () => () => {},
};
const services = createSidebarServices(backend, [backend], () => "local");
services.alias = (target) =>
  target.kind === "chat" && target.id === "atlas-api-1"
    ? "Build server"
    : "MAC";
const args: SidebarProps = {
  services,
  data,
  indicators: chatIndicators(data),
  opened: "codex-studio-2",
  open: fn(),
  newChat: fn(),
  newSharedChat: fn(),
  addProject: fn(),
  changeProject: fn(),
  projectAccount: fn(),
  creating: false,
  rename: fn(),
  remove: fn(),
  mobile: true,
  onSearch: fn(),
  close: fn(),
  markUnread: fn(),
  markingRead: new Set(),
  settings: fn(),
};
function FleetFixture({
  scheme,
  ...props
}: SidebarProps & { scheme: "light" | "dark" }) {
  const [opened, open] = useState(props.opened);
  return (
    <MantineProvider theme={theme} forceColorScheme={scheme}>
      <div className="fleet-story">
        <Sidebar
          {...props}
          opened={opened}
          open={(id) => {
            open(id);
            props.open(id);
          }}
        />
      </div>
    </MantineProvider>
  );
}
const meta = {
  title: "Sidebar/Fleet board",
  component: Sidebar,
  args,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    values.clear();
  },
} satisfies Meta<typeof Sidebar>;
export default meta;
type Story = StoryObj<typeof meta>;
export const Dark: Story = {
  render: (props) => <FleetFixture {...props} scheme="dark" />,
};
export const Light: Story = {
  render: (props) => <FleetFixture {...props} scheme="light" />,
};
export const Interactions: Story = {
  render: (props) => <FleetFixture {...props} scheme="dark" />,
  play: async ({ canvasElement }) => {
    const page = within(canvasElement);
    await expect(page.getByRole("navigation", { name: "Chats" })).toBeVisible();
    await userEvent.type(
      page.getByRole("searchbox", { name: "Filter projects and chats" }),
      "database",
    );
    await expect(
      page.getAllByText("Repair failing database migration").length,
    ).toBeGreaterThan(0);
    await expect(
      page.queryByText("Investigate mobile navigation"),
    ).not.toBeInTheDocument();
    await userEvent.clear(
      page.getByRole("searchbox", { name: "Filter projects and chats" }),
    );
    await userEvent.click(
      page.getAllByRole("button", {
        name: "Actions for Review deployment permissions",
      })[0],
    );
    await expect(
      within(document.body).getByRole("menuitem", { name: "Mark unread" }),
    ).toBeVisible();
  },
};
