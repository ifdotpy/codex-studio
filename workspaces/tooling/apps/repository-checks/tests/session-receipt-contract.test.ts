import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

const owner = readFileSync(
  "workspaces/runtime/packages/runtime-services/src/account_session_runtime.rs",
  "utf8",
);

function definition(name: string): string {
  const start = owner.search(new RegExp(`pub (?:enum|struct) ${name} \\{`));
  if (start < 0) return "";
  const brace = owner.indexOf("{", start);
  let depth = 1;
  let end = brace + 1;
  while (depth > 0 && end < owner.length) {
    if (owner[end] === "{") depth++;
    if (owner[end] === "}") depth--;
    end++;
  }
  assert.equal(depth, 0, `${name}: complete owner definition`);
  return owner.slice(start, end);
}

test("retained session receipts reject missing delivery evidence at construction", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "receipt-contract-"));
  try {
    const contract = `
#![allow(dead_code)]
#[derive(Clone, Debug, PartialEq)] pub struct Value;
pub struct ConnectionScope;
pub struct RequestTicket;
${definition("ProviderResponse")}
${definition("RequestCertainty")}
${definition("NotSentReason")}
${definition("SessionOutcome")}
${definition("SessionReceipt")}
`;
    const check = (source: string) => {
      const target = path.join(directory, "witness.rs");
      writeFileSync(target, contract + source);
      const result = spawnSync(
        "rustc",
        [
          "--edition=2024",
          "--crate-type=lib",
          "--emit=metadata",
          "--out-dir",
          directory,
          target,
        ],
        { encoding: "utf8", timeout: 10_000 },
      );
      assert.ifError(result.error);
      return result;
    };
    const missingEvidence = check(`
pub fn invalid() -> SessionReceipt {
    SessionReceipt { scope: ConnectionScope, operation: String::new(),
      ticket: None, certainty: RequestCertainty::Written, result: None,
      not_sent_reason: None, replayed: false }
}
`);
    assert.notEqual(
      missingEvidence.status,
      0,
      "a written receipt without ticket or response must not compile",
    );
    assert.match(missingEvidence.stderr, /E0560/);
    const valid = check(`
pub fn valid() -> SessionReceipt {
    SessionReceipt { scope: ConnectionScope, operation: String::new(),
      outcome: SessionOutcome::Written { ticket: RequestTicket, response: ProviderResponse::Reply(Value) },
      replayed: false }
}
pub fn before_handoff() -> SessionReceipt {
    SessionReceipt { scope: ConnectionScope, operation: String::new(),
      outcome: SessionOutcome::NotHandedOff, replayed: false }
}
`);
    assert.equal(valid.status, 0, valid.stderr);
    for (const outcome of [
      "SessionOutcome::Written { ticket: RequestTicket }",
      "SessionOutcome::Written { response: ProviderResponse::Reply(Value) }",
      "SessionOutcome::WorkerNotSent {}",
      "SessionOutcome::Unknown {}",
    ]) {
      const invalid = check(
        `pub fn invalid() -> SessionOutcome { ${outcome} }`,
      );
      assert.notEqual(invalid.status, 0, `${outcome}: required evidence`);
      assert.match(invalid.stderr, /E0063/);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
