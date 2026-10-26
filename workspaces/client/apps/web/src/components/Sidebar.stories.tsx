import { useEffect, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { useMantineColorScheme } from "@mantine/core";
import { expect, fn, userEvent, waitFor, within } from "storybook/test";
import { publishServerAliases } from "../servers/serverAliases";
import { readServerAliases } from "../servers/registry";
import Sidebar, { type SidebarProps } from "./Sidebar";
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
        ...(index === 4 && projectIndex === 0
          ? { stall: { taskId: "review-navigation", since: now - 240 } }
          : {}),
        ...(index === 2
          ? {
              commandProgress: [
                {
                  monitorId: `${project}-checks`,
                  stage: "Running focused browser checks",
                  startedAt: now - 46,
                  lastOutputLine: "Checking responsive navigation",
                },
                {
                  monitorId: `${project}-types`,
                  stage: "Checking TypeScript contracts",
                  startedAt: now - 12,
                  lastOutputLine: "No contract errors",
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
const services = createSidebarServices(
  backend,
  [backend, { ...backend, ownerId: "remote" }],
  (target) =>
    target.kind === "chat" && target.id === "atlas-api-1" ? "remote" : "local",
);
services.alias = (target) =>
  target.kind === "chat" && target.id === "atlas-api-1" ? "BLD" : "MAC";
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
  const { setColorScheme } = useMantineColorScheme();
  useEffect(() => setColorScheme(scheme), [scheme, setColorScheme]);
  return (
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
  );
}
const meta = {
  title: "Sidebar/Fleet board",
  component: Sidebar,
  args,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    values.clear();
    const previous = readServerAliases();
    publishServerAliases({ local: "MAC" });
    return () => publishServerAliases(previous);
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
export const CompactHeader: Story = {
  render: (props) => <FleetFixture {...props} scheme="dark" />,
  play: async ({ canvasElement, args }) => {
    const sidebar = canvasElement.querySelector("#sidebar")!;
    await expect(sidebar.querySelector(".sidebar-nav")).toBeNull();
    await expect(sidebar.querySelector("kbd")).toBeNull();
    const header = within(
      sidebar.querySelector<HTMLElement>(".sidebar-header")!,
    );
    const search = header.getByRole("button", { name: "Search chats" });
    await expect(search).toHaveAttribute(
      "aria-keyshortcuts",
      "Control+k Meta+k",
    );
    await expect(search).toHaveAttribute(
      "title",
      expect.stringContaining("Ctrl+K"),
    );
    await userEvent.click(search);
    await expect(args.onSearch).toHaveBeenCalledOnce();
    const projects = within(
      sidebar.querySelector<HTMLElement>(".projects-heading")!,
    );
    await userEvent.click(projects.getByRole("button", { name: "New chat" }));
    await expect(args.newChat).toHaveBeenCalledWith();
    await userEvent.click(
      projects.getByRole("button", { name: "New shared chat" }),
    );
    await expect(args.newSharedChat).toHaveBeenCalledWith();
    await userEvent.click(
      projects.getByRole("button", { name: "Add project" }),
    );
    await expect(args.addProject).toHaveBeenCalledOnce();
    await expect(
      within(sidebar as HTMLElement).getByRole("searchbox", {
        name: "Filter projects and chats",
      }),
    ).toBeVisible();
  },
};
export const Interactions: Story = {
  render: (props) => <FleetFixture {...props} scheme="dark" />,
  play: async ({ canvasElement, args }) => {
    const page = within(canvasElement);
    const lane = page.getByRole("region", { name: "Needs you" });
    for (const strip of canvasElement.querySelectorAll(".fleet-strip")) {
      if (strip.getAttribute("data-sidebar-item") === "atlas-api-1") {
        await expect(
          strip.querySelector(".fleet-server-badge"),
        ).toHaveTextContent("BLD");
      } else {
        await expect(strip.querySelector(".fleet-server-badge")).toBeNull();
      }
      await expect(
        strip.querySelector(".fleet-chat-heading .fleet-server-badge"),
      ).toBeNull();
    }
    const firstStrip = lane.querySelector(".fleet-strip")!;
    const answerStatus = firstStrip.querySelector(
      '[data-chat-status="answer"]',
    )!;
    await expect(
      answerStatus.closest(".mantine-VisuallyHidden-root"),
    ).toBeNull();
    const workingStatus = canvasElement.querySelector(
      '[data-chat-status="working"]',
    )!;
    await expect(
      workingStatus.closest(".mantine-VisuallyHidden-root"),
    ).toBeNull();
    await expect(getComputedStyle(firstStrip).boxShadow).toBe("none");
    await expect(getComputedStyle(firstStrip).backgroundColor).not.toBe(
      "rgba(0, 0, 0, 0)",
    );
    await expect(page.queryByLabelText("Elapsed time")).not.toBeInTheDocument();
    await expect(
      canvasElement.querySelector(".fleet-chat-provider"),
    ).toBeNull();
    await expect(canvasElement.querySelector(".fleet-chat-time")).toBeNull();
    await expect(
      [...lane.querySelectorAll(".fleet-strip")].map((row) =>
        row.getAttribute("data-studio-state"),
      ),
    ).toEqual(["answer", "answer", "answer", "error", "error", "error"]);
    await expect(
      page.getAllByRole("img", { name: "1 working, 1 needs you, 1 failed" }),
    ).toHaveLength(3);
    const [review] = within(lane).getAllByRole("button", {
      name: /Review deployment permissions/,
    });
    if (!review) throw new Error("Missing review deployment button");
    await userEvent.click(review);
    await expect(args.open).toHaveBeenCalledWith("codex-studio-0");
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
    const [actions] = page.getAllByRole("button", {
      name: "Actions for Review deployment permissions",
    });
    if (!actions) throw new Error("Missing review deployment actions");
    await userEvent.click(actions);
    const unread = await within(document.body).findByRole("menuitem", {
      name: "Mark unread",
    });
    await waitFor(() => expect(unread).toBeVisible());
    await expect(unread).toBeDisabled();
    await userEvent.click(
      within(document.body).getByRole("menuitem", { name: "Rename" }),
    );
    await expect(
      page.getAllByRole("textbox", { name: "Chat name" }),
    ).toHaveLength(1);
    await expect(
      within(lane).getByRole("textbox", { name: "Chat name" }),
    ).toBeVisible();
    await userEvent.click(page.getByRole("button", { name: /^Cancel$/ }));
  },
};

export const NoAttention: Story = {
  args: {
    indicators: new Map(
      [...args.indicators].filter(
        ([, indicator]) =>
          indicator.kind !== "answer" && indicator.kind !== "error",
      ),
    ),
  },
  render: (props) => <FleetFixture {...props} scheme="light" />,
  play: async ({ canvasElement }) => {
    await expect(
      within(canvasElement).queryByRole("region", { name: "Needs you" }),
    ).not.toBeInTheDocument();
  },
};

export const Offline: Story = {
  args: { offlineServers: new Set(["local", "remote"]) },
  render: (props) => <FleetFixture {...props} scheme="dark" />,
  play: async ({ canvasElement }) => {
    const badges = canvasElement.querySelectorAll<HTMLElement>(
      ".fleet-server-badge",
    );
    await expect(badges.length).toBeGreaterThan(0);
    for (const badge of badges) {
      await expect(badge).toHaveAttribute("data-offline", "true");
      await expect(badge).toHaveAccessibleName(/unreachable/);
      await expect(badge.querySelector("svg")).toBeVisible();
      const color = document.createElement("span");
      color.style.color = "var(--studio-state-error)";
      badge.append(color);
      await expect(getComputedStyle(badge).color).toBe(
        getComputedStyle(color).color,
      );
      color.remove();
    }
  },
};
