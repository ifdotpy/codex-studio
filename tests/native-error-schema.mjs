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
    nativeErrorView(
      JSON.stringify({ message: "  ", codexErrorInfo: "serverOverloaded" }),
    ).message,
    capacity.message,
  );
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
  const nestedError = {
    error: { message: "Service unavailable", code: "unavailable" },
  };
  for (const raw of [
    JSON.stringify(nestedError),
    "```json\n" + JSON.stringify(nestedError) + "\n```",
    "HTTP 503: " + JSON.stringify(nestedError) + " (request abc)",
    JSON.stringify({ error: { message: JSON.stringify(nestedError) } }),
    JSON.stringify("Service unavailable"),
  ]) {
    const view = nativeErrorView(raw);
    assert.equal(view.message, "Service unavailable");
    assert.equal(view.details, raw);
    const notification = {
      message: raw,
      codexErrorInfo: "httpConnectionFailed",
    };
    const copy = JSON.stringify(notification);
    const native = nativeErrorView(notification);
    assert.equal(native.message, "Service unavailable");
    assert.equal(native.kind, "httpConnectionFailed");
    assert.equal(native.details, JSON.stringify(notification, null, 2));
    assert.equal(JSON.stringify(notification), copy);
  }
  assert.equal(
    nativeErrorView(new Error("Error instance")).message,
    "Error instance",
  );
  const instance = new Error('HTTP 503: {"error":{"message":"Unavailable"}}');
  assert.equal(nativeErrorView(instance).message, "Unavailable");
  assert.equal(
    JSON.parse(nativeErrorView(instance).details).message,
    instance.message,
  );
  assert.equal(
    nativeErrorView({ error: { message: "Nested failure" } }).message,
    "Nested failure",
  );
  for (const raw of [
    'HTTP 400: {"error":{"code":"bio_policy","message":"Restricted"}} tail',
    '```json\n{"error":{"code":"bio_policy","message":"Restricted"}}\n```',
  ]) {
    const view = nativeErrorView({ message: raw, codexErrorInfo: "other" });
    assert.equal(view.message, "Restricted");
    assert.equal(view.title, "This content can't be shown");
    assert.equal(view.kind, "other");
  }
  for (const raw of [
    'HTTP 500: {"message":"bio_policy mentioned in a log"}',
    'HTTP 500: {"error":{"message":"Failed"},"metadata":{"code":"bio_policy"}}',
    'HTTP 500: {"error":{"message":"Failed","codexErrorInfo":"misalignmentPolicyViolation"}}',
    '{"message":"Failed","metadata":{"error":{"code":"bio_policy"}}}',
  ]) {
    const view = nativeErrorView({ message: raw, codexErrorInfo: "other" });
    assert.equal(view.kind, "other");
    assert.equal(view.title, "");
    assert.equal(view.links.length, 0);
    assert.equal(nativeThreadError({ error: { message: raw } }), undefined);
  }
  for (const raw of [
    'HTTP failure {"requestId":"abc"} tail',
    '{"requestId":"abc"}',
    'HTTP failure {"error":{"message":null}} tail',
    'HTTP failure {"error":{"message":"unterminated}',
    "HTTP failure " + "{".repeat(70_000),
  ])
    assert.equal(nativeErrorView(raw).message, raw);
  assert.equal(
    nativeErrorView(
      'HTTP error {"requestId":"abc"} then {"error":{"message":"Actual failure"}}',
    ).message,
    "Actual failure",
  );
  assert.equal(
    nativeErrorView({
      message: JSON.stringify({
        error: { message: '<img src=x onerror="alert(1)"> {text}' },
      }),
    }).message,
    '<img src=x onerror="alert(1)"> {text}',
  );
  console.log(
    `PASS: ${codes.length} installed native error variants, structured and plain errors, unknown variant fallback`,
  );
} finally {
  rmSync(dir, { recursive: true, force: true });
}
