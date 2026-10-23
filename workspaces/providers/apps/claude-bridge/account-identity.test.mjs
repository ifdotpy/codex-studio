import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  acceptsIdentity,
  createAccountCheck,
  credentialIdentity,
} from "./account-identity.mjs";
import { startBridge } from "./testing/bridge-harness.mjs";

const roots = [];
const bridges = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
  for (const root of roots.splice(0))
    fs.rmSync(root, { recursive: true, force: true });
});
const email = "a@example.test";
const account = (organizationUuid = "org-a", accountUuid = "account-a") => ({
  email,
  organizationUuid,
  accountUuid,
  apiProvider: "firstParty",
  subscriptionType: "max",
});
function fixture(expectedIdentity) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "claude-identity-"));
  roots.push(root);
  const options = {
    expectedEmail: email,
    expectedIdentity,
    identityPath: path.join(root, "pin"),
    nativePath: path.join(root, ".claude.json"),
  };
  return { options, check: createAccountCheck(options) };
}
const refuses = (fn) =>
  assert.throws(
    fn,
    (error) =>
      error.claudeAccountValidationFailed &&
      error.data.claudeAccountFailure === "identity_mismatch" &&
      error.data.turnStartOutcome === "not_applied",
  );

test("shared Claude credential identity contract matches the Rust owner", () => {
  const cases = JSON.parse(
    fs.readFileSync(
      new URL(
        "../../../runtime/packages/runtime-services/src/account_driver/claude_identity_cases.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  for (const [email, organization, uuid, expected] of cases.identities)
    assert.equal(credentialIdentity(email, organization, uuid), expected);
  for (const [old, next, expected] of cases.comparisons)
    assert.equal(
      acceptsIdentity(cases.identities[old][3], cases.identities[next][3]),
      expected,
    );
});

test("checkAccount refuses another organization or UUID with the same email", () => {
  const { check } = fixture(credentialIdentity(email, "org-a", "account-a"));
  check(account());
  refuses(() => check(account("org-b")));
  refuses(() => check(account("org-a", "account-b")));
  refuses(() => check({ ...account(), organizationUuid: undefined }));
  refuses(() => check({ ...account(), accountUuid: undefined }));
});

test("checkAccount upgrades a legacy pin once and preserves it after restart", () => {
  const { check, options } = fixture();
  const observed = check(account());
  assert.equal(
    observed._credentialIdentity,
    credentialIdentity(email, "org-a", "account-a"),
  );
  const restarted = createAccountCheck(options);
  refuses(() => restarted(account("org-b")));
  restarted(account());
  assert.equal(
    fs.readFileSync(options.identityPath, "utf8"),
    observed._credentialIdentity,
  );
});

test("checkAccount obtains native UUIDs and rejects conflicting SDK evidence", () => {
  const { check, options } = fixture();
  fs.writeFileSync(
    options.nativePath,
    JSON.stringify({
      oauthAccount: {
        emailAddress: email,
        organizationUuid: "org-a",
        accountUuid: "account-a",
      },
    }),
  );
  const observed = check({
    ...account(),
    organizationUuid: undefined,
    accountUuid: undefined,
    organization: "Display name",
  });
  assert.equal(observed.organizationUuid, "org-a");
  assert.equal(observed.accountUuid, "account-a");
  refuses(() => check(account("org-b")));
});

test("checkAccount preserves email provider and subscription refusal", () => {
  const { check } = fixture();
  for (const invalid of [
    { ...account(), email: "b@example.test" },
    { ...account(), apiProvider: "bedrock" },
    { ...account(), subscriptionType: "" },
    { ...account(), apiKeySource: "env" },
  ])
    assert.throws(
      () => check(invalid),
      (error) => error.claudeAccountValidationFailed,
    );
});

const SDK = String.raw`
export const tool=(name,description,schema,call)=>({name,description,schema,call});
export const createSdkMcpServer=value=>({...value});
export const forkSession=async()=>({sessionId:'22222222-2222-4222-8222-222222222222'});
export const getSessionMessages=async()=>[];
export function query({prompt}) {
 return {
  accountInfo:async()=>({email:'test@example.test',subscriptionType:'max',apiProvider:'firstParty'}),
  supportedModels:async()=>[{value:'default',displayName:'Default',resolvedModel:'claude-opus-5-5'}],
  initializationResult:async()=>({commands:[]}),
  close(){},
  async *[Symbol.asyncIterator](){
    const input=await prompt.next();
    if(!input.done) throw new Error('INPUT MUST NOT BE SUBMITTED');
  }
 };
}
`;
test("the real bridge reports native UUIDs and refuses input after an organization change", async () => {
  const bridge = await startBridge(SDK, "claude-org-identity-");
  bridges.push(bridge);
  const directory = path.join(bridge.root, "native-config");
  fs.mkdirSync(directory, { recursive: true });
  const profile = (organizationUuid) =>
    fs.writeFileSync(
      path.join(directory, ".claude.json"),
      JSON.stringify({
        oauthAccount: {
          emailAddress: "test@example.test",
          organizationUuid,
          accountUuid: "account-a",
        },
      }),
    );
  profile("org-a");
  const result = await bridge.call("account/read", {});
  assert.equal(result.account.organizationUuid, "org-a");
  assert.equal(result.account.accountUuid, "account-a");
  assert.equal(
    result.account._credentialIdentity,
    credentialIdentity("test@example.test", "org-a", "account-a"),
  );
  profile("org-b");
  await bridge.call("turn/start", {
    threadId: bridge.thread,
    clientUserMessageId: "org-change-input",
    input: [{ type: "text", text: "Must stay held" }],
  });
  const completed = await bridge.note(
    (note) => note.method === "turn/completed",
  );
  assert.equal(completed.params.turn.status, "failed");
  assert.equal(completed.params.turn.startOutcome, "not_applied");
  assert.equal(
    completed.params.turn.error.data.claudeAccountFailure,
    "identity_mismatch",
  );
  assert.match(completed.params.turn.error.message, /identity changed/);
});
