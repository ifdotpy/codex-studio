import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  acceptsIdentity,
  createAccountCheck,
  credentialIdentity,
} from "./account-identity.ts";
import { startBridge } from "./testing/bridge-harness.ts";

const roots: string[] = [];
const bridges: Awaited<ReturnType<typeof startBridge>>[] = [];
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
function fixture(expectedIdentity?: string) {
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
const refuses = (fn: () => unknown) =>
  assert.throws(fn, (error) => {
    assert.ok(
      error instanceof Error &&
        "claudeAccountValidationFailed" in error &&
        "data" in error,
    );
    assert.ok(
      error.data &&
        typeof error.data === "object" &&
        "claudeAccountFailure" in error.data &&
        "turnStartOutcome" in error.data,
    );
    return (
      error.claudeAccountValidationFailed === true &&
      error.data.claudeAccountFailure === "identity_mismatch" &&
      error.data.turnStartOutcome === "not_applied"
    );
  });

test("shared Claude credential identity contract matches the Rust owner", () => {
  const cases: unknown = JSON.parse(
    fs.readFileSync(
      new URL(
        "../../../runtime/packages/runtime-services/src/account_driver/claude_identity_cases.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  assert.ok(
    cases &&
      typeof cases === "object" &&
      "identities" in cases &&
      "comparisons" in cases,
  );
  assert.ok(
    Array.isArray(cases.identities) && Array.isArray(cases.comparisons),
  );
  const identities: unknown[] = cases.identities;
  const expectedIdentities: string[] = [];
  for (const row of identities) {
    assert.ok(Array.isArray(row));
    const [email, organization, uuid, expected]: unknown[] = row;
    assert.ok(typeof email === "string" && typeof expected === "string");
    assert.equal(credentialIdentity(email, organization, uuid), expected);
    expectedIdentities.push(expected);
  }
  const comparisons: unknown[] = cases.comparisons;
  for (const row of comparisons) {
    assert.ok(Array.isArray(row));
    const [old, next, expected]: unknown[] = row;
    assert.ok(
      typeof old === "number" &&
        typeof next === "number" &&
        typeof expected === "boolean",
    );
    const previous = expectedIdentities[old],
      observed = expectedIdentities[next];
    assert.ok(previous !== undefined && observed !== undefined);
    assert.equal(acceptsIdentity(previous, observed), expected);
  }
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
    'claude:v2:["a@example.test","org-a","account-a"]',
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
      (error) =>
        error instanceof Error &&
        "claudeAccountValidationFailed" in error &&
        error.claudeAccountValidationFailed === true,
    );
});

const SDK = new URL("./account-identity.fixture.mts", import.meta.url);
test("the real bridge reports native UUIDs and refuses input after an organization change", async () => {
  const bridge = await startBridge(SDK, "claude-org-identity-");
  bridges.push(bridge);
  const directory = path.join(bridge.root, "native-config");
  fs.mkdirSync(directory, { recursive: true });
  const profile = (organizationUuid: string) =>
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
    'claude:v2:["test@example.test","org-a","account-a"]',
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
