import { MCP_AUTO_BACKGROUND_MS } from "./session-policy.generated.ts";

// Claude 2.1.302: chunk-0hrc1z2k oe. Explicit duration avoids a rollout
// overriding Studio's visible setting. Native tool eligibility still applies.
export function mcpBackgroundEnvironment(session: {
  claude?: { mcpAutoBackground?: boolean | undefined } | undefined;
}): Record<string, string> {
  return {
    CLAUDE_AUTO_BACKGROUND_TASKS:
      session.claude?.mcpAutoBackground === false ? "false" : "true",
    CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS:
      session.claude?.mcpAutoBackground === false
        ? "0"
        : String(MCP_AUTO_BACKGROUND_MS),
  };
}
