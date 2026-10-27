import assert from "node:assert/strict";
import { test } from "vitest";
import ts from "typescript";
import { sdkDiagnostics } from "./testing/sdk-typecheck.ts";

test("the actual consumers compile against the pinned SDK", () => {
  assert.deepEqual(
    sdkDiagnostics([
      "provider-lifecycle.ts",
      "features.ts",
      "usage.ts",
      "sdk-message-policy.ts",
      "command-background.ts",
    ]),
    [],
  );
});

test("changing the SDK lifecycle field type breaks the actual parser", () => {
  const errors = sdkDiagnostics("provider-lifecycle.ts", (source) => {
    assert.ok(source.includes("is_error: boolean;"));
    return source.replaceAll("is_error: boolean;", "is_error: string;");
  });
  assert.ok(
    errors.some((error) =>
      error.file?.fileName.endsWith("/provider-lifecycle.ts"),
    ),
    "A trusted type predicate must not hide changed SDK field types",
  );
});

test("adding an SDK result subtype breaks the production outcome mapping", () => {
  const errors = sdkDiagnostics("provider-lifecycle.ts", (source) =>
    source.replace(
      "'error_during_execution' |",
      "'future_result' | 'error_during_execution' |",
    ),
  );
  assert.ok(
    errors.some((error) =>
      error.file?.fileName.endsWith("/provider-lifecycle.ts"),
    ),
  );
});

for (const { name, file, from, to } of [
  {
    name: "a new top-level event",
    file: "sdk-message-policy.ts",
    from: "| SDKConversationResetMessage;",
    to: "| SDKConversationResetMessage | { type: 'future_event' };",
  },
  {
    name: "a new system event",
    file: "sdk-message-policy.ts",
    from: "| SDKConversationResetMessage;",
    to: "| SDKConversationResetMessage | { type: 'system'; subtype: 'future_system' };",
  },
  {
    name: "a new assistant error",
    file: "features.ts",
    from: "| 'cloud_credential_error';",
    to: "| 'cloud_credential_error' | 'future_error';",
  },
  {
    name: "an added model token category",
    file: "usage.ts",
    from: "export declare type ModelUsage = {",
    to: "export declare type ModelUsage = { futureTokens: number;",
  },
  {
    name: "a changed model token field",
    file: "usage.ts",
    from: "inputTokens: number;",
    to: "inputTokens: string;",
  },
  {
    name: "a new task patch status",
    file: "command-background.ts",
    from: "'killed' | 'paused';",
    to: "'killed' | 'paused' | 'future_status';",
  },
]) {
  test(`${name} fails the actual SDK consumer compilation`, () => {
    const errors = sdkDiagnostics(file, (source) => {
      assert.ok(source.includes(from), `Missing mutation source ${from}`);
      return source.replace(from, to);
    });
    assert.ok(
      errors.some((error) => error.file?.fileName.endsWith("/" + file)),
      ts.formatDiagnosticsWithColorAndContext(errors, {
        getCurrentDirectory: () => process.cwd(),
        getCanonicalFileName: (name) => name,
        getNewLine: () => "\n",
      }),
    );
  });
}
