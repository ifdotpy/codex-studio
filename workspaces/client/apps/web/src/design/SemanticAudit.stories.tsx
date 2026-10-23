import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect } from "storybook/test";
import { NativeError, NativeNotice } from "../components/NativeNotice";
import type { Agent, Message } from "../types";
import { computedContrast } from "./computedContrast";
import "./foundation.css";

function NoticeAudit() {
  return (
    <main className="foundation-sheet">
      <h1>Operational notices</h1>
      {["var(--studio-page)", "var(--surface)", "var(--raised)"].map(
        (background) => (
          <section
            key={background}
            style={{ background, padding: "var(--mantine-spacing-sm)" }}
          >
            <NativeError
              agent={
                {
                  id: "notice-agent",
                  kind: "agent",
                  name: "Release lead",
                  source: "managed",
                  status: "failed",
                  created: 1,
                  error:
                    "The provider connection failed. Your input is retained.",
                } satisfies Agent
              }
            />
            {(["error", "warning"] as const).map((kind) => (
              <NativeNotice
                key={kind}
                item={
                  {
                    id: kind,
                    role: "assistant",
                    text:
                      kind === "error"
                        ? "The provider connection failed."
                        : "Review the provider warning.",
                    nativeNotice: kind,
                  } as Message
                }
              />
            ))}
            <div className="native-error warning">
              <span>Wait for model capacity.</span>
            </div>
            <div className="native-error info">
              <span>The connection recovered.</span>
            </div>
            <div className="native-notice info">
              <details>
                <summary>Review the connection result.</summary>
              </details>
            </div>
            <div className="native-policy-composer-warning">
              This request needs a policy review.
            </div>
            <div className="native-limit-notice-heading">
              <strong>Account allowance reached</strong>
            </div>
          </section>
        ),
      )}
    </main>
  );
}
const meta = {
  title: "Design/Semantic audit",
  component: NoticeAudit,
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof NoticeAudit>;
export default meta;
type Story = StoryObj<typeof meta>;
async function verifyNotices(canvasElement: HTMLElement) {
  const labels = canvasElement.querySelectorAll(
    ".native-error > span, .native-notice summary, .native-policy-composer-warning, .native-limit-notice-heading strong",
  );
  expect(labels.length).toBeGreaterThan(12);
  for (const label of labels) {
    const result = computedContrast(label);
    expect(
      result.ratio,
      `${label.textContent}: ${JSON.stringify(result)}`,
    ).toBeGreaterThanOrEqual(4.5);
  }
}
export const NoticesLight: Story = {
  globals: { colorScheme: "light" },
  play: ({ canvasElement }) => verifyNotices(canvasElement),
};
export const NoticesDark: Story = {
  globals: { colorScheme: "dark" },
  play: ({ canvasElement }) => verifyNotices(canvasElement),
};
