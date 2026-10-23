import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { ActionIcon, Button, Textarea } from "@mantine/core";
import { ArrowUp, ListEnd, Mail, Settings2, PanelLeft } from "lucide-react";
import ConversationTitle from "../shell/ConversationTitle";
import ConversationStart from "./ConversationStart";
import { ExecutionSettings } from "../agents/ExecutionSettings";
import Usage from "../Usage";
import { createServerServices } from "../../servers/serverServices";
import { ServerServicesProvider } from "../../servers/ServerServicesContext";
import type { Agent, Json } from "../../types";
import type { ChatIndicator } from "../chat-status/chatStatusModel";
import "./empty-chat-settings.css";

const indicators: ChatIndicator[] = [
  { kind: "answer", label: "Needs your answer" },
  { kind: "error", label: "Failed" },
  { kind: "working", label: "Working" },
  { kind: "unread", label: "Unread result" },
  { kind: "paused", label: "Stopped" },
  { kind: "none", label: "Ready" },
];
const titles = [
  "Обновление обработчика запросов и восстановление сессии после потери соединения",
  "Repair the deployment pipeline and keep the previous release recoverable",
  "Review permissions for parallel project workspaces",
  "Extract the shared account usage display",
  "Проверка интерфейса нескольких проектов на узком экране",
  "Prepare the next release",
];
const recentChats = indicators.map((indicator, index) => ({
  id: `chat-${index}`,
  title: titles[index],
  indicator,
}));
const lead = {
  id: "story-lead",
  rootId: "story-lead",
  name: "Release coordination",
  source: "managed",
  isLead: true,
  status: "idle",
  model: "gpt-6-astra",
  effort: "high",
  provider: "codex",
  cwd: "/projects/codex-studio",
  accountKey: "default",
  workerModel: "gpt-6-luna",
  contextUsage: { tokens: 64000, window: 200000 },
} as Agent;
const models = ["gpt-6-astra", "gpt-6-luna"].map((model) => ({
  model,
  displayName: model,
  provider: "codex",
  defaultReasoningEffort: "high",
  serviceTiers: [],
  supportedReasoningEfforts: [{ reasoningEffort: "high" }],
}));
const catalog = { models, loading: false, error: "", retry: fn() };
const limits: Json = {
  accountKey: "default",
  data: {
    accountId: "story-account",
    rateLimits: {
      planType: "team",
      primary: {
        usedPercent: 28,
        windowDurationMins: 300,
        resetsAt: 2000000000,
      },
      secondary: {
        usedPercent: 43,
        windowDurationMins: 10080,
        resetsAt: 2000000000,
      },
    },
  },
};
const sessionCost = {
  rootId: lead.id,
  pricingState: "ready",
  estimated: true,
  totalUSD: 4.28,
  unknownModels: [],
  breakdown: { providers: { codex: 4.28 } },
};
const services = createServerServices({
  id: "local",
  label: "Story fixture",
  origin: window.location.origin,
});
services.api.get = (async (path: string) => {
  if (path === "/api/session-cost") return sessionCost;
  if (path === "/api/models") return { data: models };
  throw new Error(`Unexpected story request: ${path}`);
}) as typeof services.api.get;
services.watchResourceReads = (_resources, read, failed) => {
  void read().catch(failed);
  return Object.assign(() => {}, {
    refresh: () => {
      void read().catch(failed);
    },
  });
};

const meta = {
  title: "Conversation/Dispatch chrome",
  parameters: { layout: "fullscreen" },
  args: { onSelect: fn(), onTeam: fn() },
  render: (args: { onSelect: (id: string) => void; onTeam: () => void }) => (
    <ServerServicesProvider services={services}>
      <main className="workspace" style={{ position: "fixed", inset: 0 }}>
        <header className="workspace-header simple-workspace-header">
          <ActionIcon aria-label="Toggle conversations">
            <PanelLeft size={18} />
          </ActionIcon>
          <ConversationTitle
            title={titles[0]}
            agent={lead}
            indicator={indicators[0]}
            statusText="Complete"
            team={[
              {
                id: lead.id,
                name: "Lead",
                lead: true,
                indicator: indicators[2],
              },
              ...indicators.map((indicator, index) => ({
                id: `worker-${index}`,
                name: titles[index],
                lead: false,
                indicator,
              })),
            ]}
            onTeam={args.onTeam}
          />
          <Button
            variant="subtle"
            leftSection={<Mail size={16} />}
            aria-label="Messages"
          >
            <span className="messages-toggle-label">Messages</span>
          </Button>
          <ActionIcon aria-label="Chat actions">
            <Settings2 size={18} />
          </ActionIcon>
        </header>
        <section id="conversation">
          <div id="messages">
            <ConversationStart
              projectName="codex-studio"
              location="Linux VM /projects/codex-studio"
              recentChats={recentChats}
              onSelect={args.onSelect}
              execution={
                <div className="empty-chat-settings">
                  {(["orchestrator", "worker"] as const).map((role) => (
                    <div className="empty-chat-setting" key={role}>
                      <span>
                        {role === "orchestrator" ? "Model" : "Workers"}
                      </span>
                      <ExecutionSettings
                        agent={lead}
                        catalog={catalog}
                        refresh={async () => {}}
                        settingsRow
                        initialRole={role}
                      />
                    </div>
                  ))}
                </div>
              }
            />
          </div>
          <form id="composer">
            <Textarea
              aria-label="Message"
              placeholder="Describe the work…"
              autosize
              minRows={2}
            />
            <div className="composer-bar">
              <span className="composer-timing">Send after tool calls</span>
              <div className="composer-submit-actions">
                <ActionIcon aria-label="Queue for next turn">
                  <ListEnd size={18} />
                </ActionIcon>
                <ActionIcon aria-label="Send message">
                  <ArrowUp size={18} />
                </ActionIcon>
              </div>
            </div>
          </form>
          <div className="conversation-composer-footer">
            <Usage
              agent={lead}
              stateDir="story-dispatch"
              limits={limits}
              accountLabel="Engineering"
              reload={fn()}
            />
          </div>
        </section>
      </main>
    </ServerServicesProvider>
  ),
} satisfies Meta<{ onSelect: (id: string) => void; onTeam: () => void }>;
export default meta;
type Story = StoryObj<typeof meta>;
export const ProjectStart: Story = {
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await expect(
      canvas.getByRole("heading", { name: "codex-studio" }),
    ).toBeVisible();
    await expect(
      canvas.getByText("Linux VM /projects/codex-studio"),
    ).toBeVisible();
    await expect(
      canvas.getByRole("progressbar", { name: "Context used" }),
    ).toHaveAttribute("aria-valuenow", "32");
    await expect(
      canvas.getByRole("progressbar", { name: "Allowance remaining" }),
    ).toHaveAttribute("aria-valuenow", "57");
    await userEvent.click(canvas.getByRole("button", { name: /Open team/ }));
    await expect(args.onTeam).toHaveBeenCalledOnce();
    await userEvent.click(
      within(
        canvas.getByRole("navigation", { name: "Recent project chats" }),
      ).getByRole("button", { name: /Prepare the next release/ }),
    );
    await expect(args.onSelect).toHaveBeenCalledWith("chat-5");
  },
};
export const SeveralProjects: Story = {
  render: (args) => (
    <div>
      {["codex-studio", "af-utils", "attar"].map((projectName) => (
        <ConversationStart
          key={projectName}
          projectName={projectName}
          location={`/projects/${projectName}`}
          recentChats={recentChats}
          onSelect={args.onSelect}
        />
      ))}
    </div>
  ),
};
