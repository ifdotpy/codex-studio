import type { Meta, StoryObj } from "@storybook/react-vite";
import { NumberInput, TextInput, useMantineColorScheme } from "@mantine/core";
import { expect, within } from "storybook/test";
import type { ChatIndicatorKind } from "../components/chat-status/chatStatusModel";
import { type CSSProperties, useEffect } from "react";
import { ActionButton } from "../components/ui/primitives";
import "../components/realtime-voice.css";
import "./foundation.css";

const projects = [
  {
    name: "codex-studio",
    title: "Coordinate six agents across the renderer redesign",
  },
  {
    name: "af-utils",
    title:
      "Обновление обработки запросов и восстановление состояния после перезапуска",
  },
  {
    name: "attar",
    title: "Review deployment evidence before the next release",
  },
];
const states = {
  answer: "Needs your answer",
  error: "Build failed",
  working: "Running focused tests",
  unread: "Result ready to read",
  paused: "Stopped",
  none: "Ready",
} satisfies Record<ChatIndicatorKind, string>;

function Foundation({ scheme }: { scheme: "light" | "dark" }) {
  const { setColorScheme } = useMantineColorScheme();
  useEffect(() => setColorScheme(scheme), [scheme, setColorScheme]);
  return (
    <main className="foundation-sheet">
      <header>
        <h1>Dispatch foundation</h1>
        <p>Three projects. Eighteen agents. One place to see what needs you.</p>
      </header>
      <nav aria-label="Foundation navigation">
        <a href="#projects">Project overview</a>
      </nav>
      <p className="prose">
        <a href="#evidence">Review evidence</a>
      </p>
      <section aria-label="Palette" className="foundation-palette">
        {["Page", "Surface", "Ink", "Signal"].map((name) => (
          <div
            className={`foundation-swatch foundation-swatch-${name.toLowerCase()}`}
            key={name}
          >
            <span>{name}</span>
          </div>
        ))}
      </section>
      <section aria-label="Signal palette" className="foundation-signal-ramp">
        {Array.from({ length: 10 }, (_, shade) => (
          <div
            key={shade}
            style={
              {
                "--swatch": `var(--mantine-color-signal-${shade})`,
              } as CSSProperties
            }
          >
            <i aria-hidden="true" />
            <span>{shade}</span>
          </div>
        ))}
      </section>
      <section aria-label="Type scale" className="foundation-type">
        <span className="foundation-section">Projects in motion</span>
        <span className="foundation-title">A clear handoff between agents</span>
        <span>
          Review the result, answer a question, or start the next task.
        </span>
        <span className="foundation-meta studio-tabular">
          18 agents / 12 running / $24.80 / 03:42
        </span>
      </section>
      <section aria-label="Project states" className="foundation-projects">
        {projects.map((project) => (
          <section key={project.name}>
            <h2>{project.name}</h2>
            {Object.entries(states).map(([kind, label], index) => (
              <div
                className="foundation-strip"
                data-studio-state={kind}
                key={kind}
              >
                <span
                  className="studio-state-dot"
                  data-studio-state={kind}
                  aria-hidden="true"
                />
                <div className="foundation-copy">
                  <strong>
                    {index === 0 ? project.title : `Worker ${index + 1}`}
                  </strong>
                  <small>{label}</small>
                </div>
                <span className="foundation-meta studio-tabular">
                  {index + 1}m
                </span>
              </div>
            ))}
          </section>
        ))}
      </section>
      <section aria-label="Native controls" className="foundation-controls">
        <TextInput label="Task name" defaultValue="Verify the renderer" />
        <NumberInput label="Workers" defaultValue={6} min={1} max={32} />
        <ActionButton actionRole="primary">Start task</ActionButton>
      </section>
      <section className="realtime-voice" aria-label="Voice preview">
        <div className="realtime-voice-heading">
          <strong>Voice session</strong>
          <span>Ready</span>
        </div>
        <p className="realtime-voice-note">
          Review the latest results with your lead.
        </p>
        <textarea
          aria-label="Voice instruction"
          defaultValue="Summarize the three projects and ask what needs my decision."
        />
        <div className="realtime-voice-actions">
          <button type="button">Connect</button>
          <button type="button" disabled>
            Disconnect
          </button>
        </div>
      </section>
    </main>
  );
}

const meta = {
  title: "Design/Foundation",
  component: Foundation,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof Foundation>;
export default meta;
type Story = StoryObj<typeof meta>;
async function verifyFoundation(canvasElement: HTMLElement) {
  const canvas = within(canvasElement);
  expect(canvas.getAllByText("Needs your answer")).toHaveLength(3);
  expect(canvas.getAllByText("Ready", { exact: true })).toHaveLength(4);
  expect(canvas.getByLabelText("Workers")).toHaveValue("6");
  const navigation = canvas.getByRole("link", { name: "Project overview" });
  expect(getComputedStyle(navigation).color).toBe(
    getComputedStyle(navigation.parentElement!).color,
  );
  const content = canvas.getByRole("link", { name: "Review evidence" });
  expect(getComputedStyle(content).color).not.toBe(
    getComputedStyle(navigation).color,
  );
  const action = canvas.getByRole("button", { name: "Start task" });
  const signal = canvasElement.querySelector(".foundation-swatch-signal")!;
  expect(getComputedStyle(action).backgroundColor).toBe(
    getComputedStyle(signal).backgroundColor,
  );
  const luminance = (color: string) => {
    const channels = color
      .match(/[\d.]+/g)!
      .slice(0, 3)
      .map(Number)
      .map((v) => v / 255)
      .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
    return channels.reduce(
      (sum, channel, index) => sum + channel * [0.2126, 0.7152, 0.0722][index],
      0,
    );
  };
  const foreground = luminance(getComputedStyle(action).color);
  const background = luminance(getComputedStyle(action).backgroundColor);
  expect(
    (Math.max(foreground, background) + 0.05) /
      (Math.min(foreground, background) + 0.05),
  ).toBeGreaterThanOrEqual(4.5);
}
export const Light: Story = {
  args: { scheme: "light" },
  play: async ({ canvasElement }) => verifyFoundation(canvasElement),
};
export const Dark: Story = {
  args: { scheme: "dark" },
  play: async ({ canvasElement }) => verifyFoundation(canvasElement),
};
