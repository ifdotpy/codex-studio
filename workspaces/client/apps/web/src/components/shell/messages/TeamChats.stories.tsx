import type { Meta, StoryObj } from "@storybook/react-vite";
import { MantineProvider } from "@mantine/core";
import { expect, userEvent, within } from "storybook/test";
import TeamChats from "./TeamChats";
import { theme } from "../../../theme";
import type { Snapshot } from "../../../types";

const data = {
  token: "storybook",
  stateDir: "storybook-messages",
  threads: [],
  chats: [],
  nodes: [],
  edges: [],
  runtime: {
    agents: [],
    rooms: [],
    tasks: [],
    monitors: [],
    complaints: [],
    requests: [],
    rules: [],
    projects: [],
    peerTeams: [],
    events: [],
    work: [],
  },
} satisfies Snapshot;

const populated: Snapshot = {
  ...data,
  threads: [
    { id: "lead", name: "Release lead" },
    { id: "worker", rootId: "lead", name: "API tests" },
  ],
  runtime: {
    ...data.runtime,
    rooms: [
      {
        id: "broadcast:lead",
        kind: "broadcast",
        name: "Team broadcast",
        rootId: "lead",
        lastMessage: {
          created: 1792706400,
          sender: "lead",
          seq: 1,
          text: "Release checks are complete. Review the API changes.",
        },
      },
      {
        id: "private:worker",
        kind: "private",
        name: "API tests",
        members: ["lead", "worker"],
        lastMessage: {
          created: 1792706400,
          sender: "worker",
          seq: 1,
          text: "The retry test reproduces the missing receipt.",
        },
      },
    ],
  },
};
function MessagesFixture({
  light = false,
  hasRooms = false,
  narrow = false,
}: {
  light?: boolean;
  hasRooms?: boolean;
  narrow?: boolean;
}) {
  return (
    <MantineProvider theme={theme} forceColorScheme={light ? "light" : "dark"}>
      <div
        style={{
          display: "flex",
          height: "80dvh",
          minWidth: 0,
          maxWidth: narrow ? "var(--studio-form-max, 40rem)" : undefined,
        }}
      >
        <TeamChats
          data={hasRooms ? populated : data}
          leadId="lead"
          refresh={async () => {}}
          notify={() => {}}
        />
      </div>
    </MantineProvider>
  );
}
const meta = {
  title: "Messages/Room list",
  component: MessagesFixture,
  parameters: { layout: "fullscreen" },
  beforeEach: () => {
    const original = window.fetch;
    window.fetch = async (input, init) => {
      const request = new Request(input, init);
      if (new URL(request.url).pathname === "/api/agent-chat")
        return new Response(
          JSON.stringify({
            messages: [
              {
                id: "message-1",
                sender: "worker",
                senderName: "API tests",
                created: 1792706400,
                seq: 1,
                text: "The retry test reproduces the missing receipt. I am checking the request identity.",
              },
            ],
            nextBefore: null,
            nextAfter: 1,
          }),
          { headers: { "content-type": "application/json" } },
        );
      return original(input, init);
    };
    return () => {
      window.fetch = original;
    };
  },
} satisfies Meta<typeof MessagesFixture>;
export default meta;
type Story = StoryObj<typeof meta>;
export const EmptyDark: Story = {};
export const EmptyLight: Story = { args: { light: true } };
export const RoomsDark: Story = { args: { hasRooms: true } };
export const RoomsLight: Story = { args: { hasRooms: true, light: true } };
export const NarrowDrawer: Story = {
  args: { hasRooms: true, narrow: true },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(
      canvas.getByRole("textbox", { name: "Search chats" }),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: /API tests The retry test/ }),
    );
    await expect(
      await canvas.findByText(
        "The retry test reproduces the missing receipt. I am checking the request identity.",
      ),
    ).toBeVisible();
    await expect(
      canvas.getByRole("heading", { name: "API tests" }),
    ).toHaveFocus();
    await userEvent.click(
      canvas.getByRole("button", { name: "Back to chats" }),
    );
    await expect(
      canvas.getByRole("textbox", { name: "Search chats" }),
    ).toBeVisible();
  },
};
