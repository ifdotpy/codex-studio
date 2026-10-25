import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.mjs";
import { claudeFailure, terminalClaudeFailure } from "./failure-causes.mjs";

const SDK = String.raw`
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const getSessionMessages=async()=>[];
export const forkSession=async()=>({sessionId:"22222222-2222-4222-8222-222222222222"});
export function query({prompt}){
 const abort=new AbortController();
 return {
  mcpServerStatus:async()=>[],supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'Claude Max',apiProvider:'firstParty'}),
  initializationResult:async()=>({commands:[]}),setModel:async()=>{},setPermissionMode:async()=>{},applyFlagSettings:async()=>{},
  setMcpServers:async()=>({added:[],removed:[],errors:{}}),close(){abort.abort();},interrupt:async()=>abort.abort(),
  async *[Symbol.asyncIterator](){
   const input=await prompt.next(); if(input.done)return;
   const config=JSON.parse(input.value.message.content[0].text);
   yield {type:'user',isReplay:true,uuid:input.value.uuid};
   yield {type:'system',subtype:'init'};
   if(config.thrown)throw Object.assign(new Error('Provider failed'),config.thrown);
   if(config.assistant) yield {type:'assistant',uuid:'failure',parent_tool_use_id:null,message:{id:'failure',model:'<synthetic>',usage:{input_tokens:0,output_tokens:0},content:[{type:'text',text:'Provider failed'}]},...config.assistant};
   yield {type:'result',subtype:'success',is_error:true,usage:{input_tokens:0,output_tokens:0},result:'Provider failed',...config.result};
   await new Promise(resolve=>abort.signal.addEventListener('abort',resolve,{once:true}));
  }
 };
}
`;
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});
async function failure(config) {
  const bridge = await startBridge(SDK, "claude-failure-");
  bridges.push(bridge);
  const { turn } = await bridge.call("turn/start", {
    threadId: bridge.thread,
    input: [{ type: "text", text: JSON.stringify(config) }],
  });
  const done = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn.id,
  );
  assert.equal(done.params.turn.status, "failed");
  return done.params.turn.error;
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
    assert.equal(error.claudeFailure.error, errorClass);
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
