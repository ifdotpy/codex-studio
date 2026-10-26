import type { BridgeHarness } from "./testing/bridge-harness.ts";
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.ts";
import { claudeFailure, terminalClaudeFailure } from "./failure-causes.ts";

// Scripted SDK failures reach the real bridge; raw text and secrets must not be retained.
const FAKE_SDK = new URL("./failure-causes.fixture.mts", import.meta.url);

const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function failure(config: Record<string, unknown>) {
  const bridge = await startBridge(FAKE_SDK, "claude-failure-");
  bridges.push(bridge);
  const { turn } = (await bridge.call("turn/start", {
    threadId: bridge.thread,
    input: [{ type: "text", text: JSON.stringify(config) }],
  })) as { turn: { id: string } };
  const done = await bridge.note(
    "turn/completed",
    (n) => n.params.turn.id === turn.id,
  );
  assert.equal(done.params.turn.status, "failed");
  const error = done.params.turn.error;
  assert.ok(error);
  return error;
}

const causes = [
  "max_output_tokens",
  "dlp_request_denied",
  "claude_code_version_too_old",
  "safety_monitor_blocked",
  "effort_requires_thinking",
  "advisor_incompatible",
  "tool_history_mismatch",
  "autocompact_thrashing",
  "pdf_too_large",
  "pdf_password_protected",
  "media_removed",
  "no_response",
  "tls_untrusted_ca",
  "gateway_content_type",
  "provider_credentials",
  "gateway_signin_required",
  "gateway_session_expired",
  "api_key_auth_disabled",
  "org_disabled_credential",
  "invalid_credential_header",
  "model_requires_usage_credits",
  "long_context_credits_required",
  "consent_unanswered",
  "no_allowed_fallback",
  "model_substitution_disabled",
  "field_not_granted",
  "usage_limit_reached",
];

for (const cause of causes)
  test(`retains ${cause} without raw text or secrets`, async () => {
    const error = await failure({
      assistant: {
        error: "invalid_request",
        api_error: cause,
        api_error_status: 400,
        api_error_code: "new_server_gate",
        error_details:
          "prompt is too long: 210001 tokens > 200000 maximum, token=sk-secret",
        api_error_params: {
          provider: "bedrock",
          remedy: "refresh_credentials",
          media: "document",
          media_reason: "media_budget",
          token: "sk-secret",
        },
      },
    });
    assert.deepEqual(error.claudeFailure, {
      error: "invalid_request",
      apiError: cause,
      apiErrorStatus: 400,
      apiErrorCode: "new_server_gate",
      provider: "bedrock",
      remedy: "refresh_credentials",
      media: "document",
      mediaReason: "media_budget",
      promptTokens: 210001,
      maxContextTokens: 200000,
    });
    assert.ok(!JSON.stringify(error.claudeFailure).includes("secret"));
  });

for (const errorClass of [
  "authentication_failed",
  "oauth_org_not_allowed",
  "account_on_hold",
  "verification_required",
  "billing_error",
  "rate_limit",
  "overloaded",
  "invalid_request",
  "model_not_found",
  "server_error",
  "unknown",
  "max_output_tokens",
  "cloud_credential_error",
])
  test(`retains the native ${errorClass} class`, async () => {
    const error = await failure({ assistant: { error: errorClass } });
    assert.equal(error.claudeFailure?.error, errorClass);
    if (errorClass === "authentication_failed")
      assert.equal(error.codexErrorInfo, "unauthorized");
    if (errorClass === "rate_limit")
      assert.equal(error.codexErrorInfo, "rateLimitExceeded");
  });

test("a result without an assistant error retains its own cause", async () => {
  const error = await failure({
    result: {
      api_error: "media_removed",
      api_error_status: 422,
      api_error_code: "media_gate",
      api_error_origin: "first_party",
    },
  });
  assert.deepEqual(error.claudeFailure, {
    apiError: "media_removed",
    apiErrorStatus: 422,
    apiErrorCode: "media_gate",
    apiErrorOrigin: "first_party",
  });
});

test("a thrown SDK error retains the structured cause", async () => {
  const error = await failure({
    thrown: {
      error: "authentication_failed",
      api_error: "provider_credentials",
      api_error_status: 401,
    },
  });
  assert.deepEqual(error.claudeFailure, {
    error: "authentication_failed",
    apiError: "provider_credentials",
    apiErrorStatus: 401,
  });
});

test("invalid and unbounded values cannot enter the metadata", () => {
  for (const raw of ["sk-secret", "Bearer private", "x".repeat(65), "bad.code"])
    assert.equal(
      claudeFailure({
        api_error_code: raw,
        api_error_status: 999,
        error_details: "private prompt",
        api_error: "future_unknown",
        api_error_params: { provider: "secret", remedy: "secret" },
      }),
      undefined,
    );
  assert.equal(
    claudeFailure({
      error_details:
        "prompt is too long: 9999999999999999999999 tokens > 9007199254740992 maximum",
    }),
    undefined,
  );
  assert.equal(claudeFailure({ error_details: "x".repeat(65537) }), undefined);
  assert.deepEqual(
    terminalClaudeFailure({ apiError: "media_removed", media: "image" }, null, {
      api_error: "provider_credentials",
    }),
    { apiError: "provider_credentials" },
  );
});

test("terminal non-API failures do not reuse an earlier HTTP failure", () => {
  assert.equal(
    terminalClaudeFailure(
      { apiError: "media_removed", apiErrorStatus: 400 },
      null,
      { api_error_status: null },
    ),
    undefined,
  );
  assert.deepEqual(
    terminalClaudeFailure(
      {
        apiError: "autocompact_thrashing",
        apiErrorStatus: 400,
        apiErrorOrigin: "first_party",
      },
      null,
      { api_error: "autocompact_thrashing", api_error_status: null },
    ),
    { apiError: "autocompact_thrashing" },
  );
});
