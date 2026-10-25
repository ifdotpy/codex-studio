import { useState } from "react";
import { Button } from "@mantine/core";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within, waitFor } from "storybook/test";
import ConversationWarnings from "./ConversationWarnings";
import { ServerServicesProvider } from "../servers/ServerServicesContext";
import type { ServerServices } from "../servers/serverServices";
import { StudioErrorsProvider, type StudioErrorItem } from "./StudioErrors";

const errors: StudioErrorItem[] = ["one", "two", "three"].map((id) => ({
  id: `monitor:${id}`,
  kind: "monitor_outcome_lost",
  reason: "Studio lost the command result.",
  action: "check",
  agentId: id,
  agentName: `Worker ${id}`,
  monitorId: id,
  eventId: null,
  detail: "The command may have run. Studio does not run it again.",
  subject: "cargo test",
  count: 1,
  firstAt: 1792700000,
  at: 1792700000,
}));
let items = [...errors];
const dismissed = fn(async (_path: string, body: { ids: string[] }) => {
  items = items.filter((item) => !body.ids.includes(item.id));
  return { items, dismissedIds: body.ids, unknownIds: [] };
});
const read = fn(async () => ({ items }));
const services = {
  owner: { id: "fixture", origin: "http://fixture", credentialId: null },
  api: { get: read, post: dismissed },
} as unknown as ServerServices;
const meta = {
  title: "Sidebar/Warnings",
  component: ConversationWarnings,
  decorators: [
    (Story) => (
      <ServerServicesProvider services={services}>
        <StudioErrorsProvider items={errors}>
          <Story />
        </StudioErrorsProvider>
      </ServerServicesProvider>
    ),
  ],
  args: {
    scope: "fixture",
    accountKey: "default",
    messages: [],
    notices: [
      {
        id: "provider-update:default",
        accountKey: "default",
        provider: "codex",
        version: "0.154.0",
        baseline: "0.153.4",
        message: "A newer Codex version is available.",
        at: 1,
        level: "info",
      },
    ],
  },
  beforeEach: () => {
    items = [...errors];
    dismissed.mockClear();
    read.mockClear();
  },
} satisfies Meta<typeof ConversationWarnings>;
export default meta;
type Story = StoryObj<typeof meta>;

export const SelectAndDelete: Story = {
  play: async () => {
    const page = within(document.body);
    await userEvent.click(
      await page.findByRole("button", { name: "Warnings" }),
    );
    await userEvent.click(await page.findByRole("tab", { name: /App errors/ }));
    const remove = page.getByRole("button", { name: "Delete selected" });
    await expect(remove).toBeDisabled();
    await userEvent.click(page.getByRole("button", { name: "Select all" }));
    await expect(page.getAllByRole("checkbox", { checked: true })).toHaveLength(
      3,
    );
    await userEvent.click(
      page.getByRole("checkbox", {
        name: "Select error monitor:two for Worker two",
      }),
    );
    await userEvent.click(remove);
    await expect(
      await page.findByRole("checkbox", {
        name: "Select error monitor:two for Worker two",
      }),
    ).not.toBeChecked();
    await waitFor(() =>
      expect(dismissed.mock.calls).toEqual([
        [
          "/api/studio-errors/dismiss",
          { ids: ["monitor:one", "monitor:three"] },
        ],
      ]),
    );
    await expect(page.getAllByRole("checkbox")).toHaveLength(1);
    await userEvent.click(page.getByRole("tab", { name: /Notices/ }));
    await expect(
      page.getByText("A newer Codex version is available."),
    ).toBeVisible();
  },
};

export const FailedBatchKeepsSelection: Story = {
  play: async () => {
    const page = within(document.body);
    await userEvent.click(
      await page.findByRole("button", { name: "Warnings" }),
    );
    await userEvent.click(await page.findByRole("tab", { name: /App errors/ }));
    await userEvent.click(page.getByRole("button", { name: "Select all" }));
    let reject: (error: Error) => void = () => {};
    dismissed.mockImplementationOnce(
      () =>
        new Promise((_resolve, fail) => {
          reject = fail;
        }),
    );
    const readsBefore = read.mock.calls.length;
    await userEvent.click(
      page.getByRole("button", { name: "Delete selected" }),
    );
    await expect(
      page.getByRole("button", { name: "Select all" }),
    ).toBeDisabled();
    for (const checkbox of page.getAllByRole("checkbox"))
      await expect(checkbox).toBeDisabled();
    reject(new Error("Response lost"));
    await expect(await page.findByRole("alert")).toHaveTextContent(
      "Response lost",
    );
    await waitFor(() => expect(read.mock.calls.length).toBe(readsBefore + 1));
    await expect(page.getAllByRole("checkbox", { checked: true })).toHaveLength(
      3,
    );
    await expect(dismissed).toHaveBeenCalledTimes(1);
  },
};

export const PendingBatchSurvivesChatChange: Story = {
  render: (args) => {
    const [chat, setChat] = useState(0);
    return (
      <>
        <Button onClick={() => setChat((value) => value + 1)}>
          Switch chat
        </Button>
        <ConversationWarnings {...args} key={chat} scope={`chat:${chat}`} />
      </>
    );
  },
  play: async () => {
    const page = within(document.body);
    await userEvent.click(
      await page.findByRole("button", { name: "Warnings" }),
    );
    await userEvent.click(await page.findByRole("tab", { name: /App errors/ }));
    await userEvent.click(page.getByRole("button", { name: "Select all" }));
    let reject: (error: Error) => void = () => {};
    dismissed.mockImplementationOnce(
      () =>
        new Promise((_resolve, fail) => {
          reject = fail;
        }),
    );
    await userEvent.click(
      page.getByRole("button", { name: "Delete selected" }),
    );
    await userEvent.keyboard("{Escape}");
    await userEvent.click(page.getByRole("button", { name: "Switch chat" }));
    await userEvent.click(
      await page.findByRole("button", { name: "Warnings" }),
    );
    await userEvent.click(await page.findByRole("tab", { name: /App errors/ }));
    await expect(
      page.getByRole("button", { name: "Select all" }),
    ).toBeDisabled();
    await expect(page.getAllByRole("checkbox", { checked: true })).toHaveLength(
      3,
    );
    reject(new Error("Response lost after switching chats"));
    await expect(await page.findByRole("alert")).toHaveTextContent(
      "Response lost after switching chats",
    );
    await expect(dismissed).toHaveBeenCalledTimes(1);
  },
};
