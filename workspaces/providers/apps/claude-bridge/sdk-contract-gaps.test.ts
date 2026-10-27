import assert from "node:assert/strict";
import { test } from "vitest";
import { sdkDiagnostics } from "./testing/sdk-typecheck.ts";

test("the exact two vendor gaps compile against the pinned declarations", () => {
  assert.deepEqual(sdkDiagnostics("sdk-contract-gaps.ts"), []);
});

for (const { name, from, to } of [
  {
    name: "proxy config acceptance",
    from: "McpHttpServerConfig | McpSdkServerConfigWithInstance;",
    to: "McpHttpServerConfig | McpSdkServerConfigWithInstance | McpClaudeAIProxyServerConfig;",
  },
  {
    name: "query flag overlays",
    from: "settings?: string | Settings;",
    to: "settings?: string | Parameters<Query['applyFlagSettings']>[0];",
  },
]) {
  test(`an SDK repair of ${name} requires removing its narrow expected error`, () => {
    const errors = sdkDiagnostics("sdk-contract-gaps.ts", (source) => {
      assert.ok(source.includes(from));
      return source.replace(from, to);
    });
    assert.ok(
      errors.some(
        (error) =>
          error.code === 2578 &&
          error.file?.fileName.endsWith("/sdk-contract-gaps.ts"),
      ),
    );
  });
}
