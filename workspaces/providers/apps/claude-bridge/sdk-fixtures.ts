import { waitForFile as waitForFileSignal } from "./testing/file-signals.ts";
import type { Options, SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";

// Only the SDK surface used by these scripted bridge scenarios is implemented.
export type FixtureQuery = {
  prompt: AsyncGenerator<SDKUserMessage>;
  options: Options & { cwd: string };
};
export const tool = (
  name: string,
  description: string,
  schema: unknown,
  call: unknown,
) => ({ name, description, schema, call });
export const createSdkMcpServer = (value: Record<string, unknown>) => ({
  ...value,
});
export const forkSession = async () => ({
  sessionId: "22222222-2222-4222-8222-222222222222",
});
export const getSessionMessages = async () => [];
export function inputText(input: SDKUserMessage) {
  const content = input.message.content;
  const first = typeof content === "string" ? undefined : content[0];
  return first?.type === "text" ? first.text : undefined;
}

// Scripted SDK waits keep the child IPC channel alive until their signal settles.
export function waitForFile(
  file: string,
  options: Parameters<typeof waitForFileSignal>[1] = {},
) {
  return waitForFileSignal(file, { ...options, childIpc: true });
}
