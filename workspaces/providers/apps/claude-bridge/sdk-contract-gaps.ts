import type {
  McpServerConfig,
  McpClaudeAIProxyServerConfig,
  Options,
  Query,
} from "@anthropic-ai/claude-agent-sdk";

// SDK 0.3.308 exports the proxy config and forwards it unchanged through its
// process transport, but omits it from Options.mcpServers. Keep the vendor's
// exact types and object identity. The mutation witness requires this directive
// to become unused when the SDK's accepted union includes its exported proxy.
export function sdkMcpServers(
  configs: Record<string, McpServerConfig | McpClaudeAIProxyServerConfig>,
): NonNullable<Options["mcpServers"]> {
  // @ts-expect-error SDK 0.3.308 accepts its exported proxy at runtime but omits it from McpServerConfig.
  return configs;
}

// The process accepts the flag overlay that applyFlagSettings types directly,
// including null clears and session-only effort. Options.settings incorrectly
// names persisted Settings instead. No fields or values are rewritten here.
export function sdkSettings(
  value: Parameters<Query["applyFlagSettings"]>[0],
): NonNullable<Options["settings"]> {
  // @ts-expect-error SDK 0.3.308 query settings accepts flag overlays, not only persisted Settings.
  return value;
}
