// Compare UI coverage to the installed native protocol without model requests.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  nativeErrorHints,
  nativeErrorView,
  nativeErrorKind,
  nativeThreadError,
} from "../web/src/nativeErrors.ts";
const dir = mkdtempSync(join(tmpdir(), "studio-error-schema-"));
try {
  execFileSync(process.env.CODEX_BIN || "codex", [
    "app-server",
    "generate-json-schema",
    "--experimental",
    "--out",
    dir,
  ]);
  const schema = JSON.parse(
    readFileSync(join(dir, "v2/ErrorNotification.json"), "utf8"),
  );
  const codes = schema.definitions.CodexErrorInfo.oneOf.flatMap(
    (v) => v.enum || Object.keys(v.properties),
  );
  assert.deepEqual([...codes].sort(), Object.keys(nativeErrorHints).sort());
  for (const code of codes) {
    for (const info of [code, { [code]: { httpStatusCode: 503 } }]) {
      const view = nativeErrorView({
        message: "Native message",
        codexErrorInfo: info,
        additionalDetails: "Cause",
      });
      assert.equal(view.message, "Native message");
      assert.equal(view.kind, code);
      assert.ok(view.hint.length > 0);
      assert.ok(view.details.includes("Cause"));
    }
  }
  assert.equal(
    nativeErrorView('{"message":"RPC rejected","code":-32600}').message,
    "RPC rejected",
  );
  assert.equal(nativeErrorView("ordinary failure").message, "ordinary failure");
  assert.ok(
    nativeErrorView({ message: "New variant", codexErrorInfo: "futureCode" })
      .hint,
  );
  assert.equal(
    nativeErrorKind({ data: { codexErrorInfo: "serverOverloaded" } }),
    "serverOverloaded",
  );
  const capacity = nativeErrorView({
    message: "  ",
    codexErrorInfo: "serverOverloaded",
  });
  assert.equal(capacity.message, "Codex is currently experiencing high load.");
  assert.equal(capacity.severity, "warning");
  assert.equal(
    nativeErrorView({
      codexErrorInfo: { activeTurnNotSteerable: { turnKind: "compact" } },
    }).severity,
    "warning",
  );
  const policy = {
    message: "Policy error",
    codexErrorInfo: "misalignmentPolicyViolation",
  };
  assert.equal(nativeErrorView(policy).title, "Chat stopped as a precaution");
  assert.equal(
    nativeThreadError({
      threadId: "one",
      nativeThreadBlock: { threadId: "one", error: policy },
      error: "Connection lost",
    }),
    policy,
  );
  assert.equal(
    nativeThreadError({
      threadId: "two",
      nativeThreadBlock: { threadId: "one", error: policy },
    }),
    undefined,
  );
  for (const message of [
    "Invalid prompt: we've limited access to this content for safety reasons. More context",
    "This content was flagged for possible biological risk.",
    JSON.stringify({ error: { code: "bio_policy", message: "Restricted" } }),
  ]) {
    const view = nativeErrorView({ message, codexErrorInfo: "other" });
    assert.equal(view.title, "This content can't be shown");
    assert.equal(view.severity, "info");
    assert.equal(
      view.links[0].href,
      "https://chatgpt.com/r/b749fb02595e04c3007a54375f3f4374",
    );
  }
  assert.equal(
    nativeErrorView({ message: "Log text mentions bio_policy only." }).links
      .length,
    0,
  );
  for (const [plan, link] of [
    ["pro", "https://chatgpt.com/cyber/"],
    [
      "enterprise",
      "https://openai.com/form/enterprise-trusted-access-for-cyber/",
    ],
    [undefined, "https://openai.com/form/enterprise-trusted-access-for-cyber/"],
  ])
    assert.equal(
      nativeErrorView({ codexErrorInfo: "cyberPolicy" }, plan).links[0].href,
      link,
    );
  console.log(
    `PASS: ${codes.length} installed native error variants, structured and plain errors, unknown variant fallback`,
  );
} finally {
  rmSync(dir, { recursive: true, force: true });
}
