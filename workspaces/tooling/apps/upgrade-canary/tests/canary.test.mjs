import assert from "node:assert/strict";
import { test } from "node:test";
import os from "node:os";
import path from "node:path";

import {
  FAILURE_LINES,
  handleChanges,
  profiles,
  proxyDecision,
} from "../canary.mjs";

test("the proxy refuses the Codex token endpoint and every unknown host", () => {
  assert.equal(proxyDecision("auth.openai.com"), "refused");
  assert.equal(proxyDecision("AUTH.OPENAI.COM."), "refused");
  assert.equal(proxyDecision("chatgpt.com"), "allowed");
  assert.equal(proxyDecision("api.openai.com"), "allowed");
  assert.equal(proxyDecision("api.anthropic.com"), "allowed");
  assert.equal(proxyDecision("example.com"), "refused");
  assert.equal(proxyDecision("chatgpt.com.example.com"), "refused");
});

test("a handle with another pid or generation is a change", () => {
  const before = ["account:a|10|1", "account:b|20|4"];
  assert.deepEqual(handleChanges(before, [...before]), []);
  assert.deepEqual(
    handleChanges(before, ["account:a|10|1", "account:b|21|5"]),
    ["- account:b|20|4", "+ account:b|21|5"],
  );
  assert.deepEqual(handleChanges(before, ["account:a|10|1"]), [
    "- account:b|20|4",
  ]);
});

test("the failure lines match both outage messages", () => {
  assert.match("Provider did not start: refused", FAILURE_LINES);
  assert.match("The retained handle could not be verified", FAILURE_LINES);
  assert.doesNotMatch("turn started", FAILURE_LINES);
});

test("every canary sandbox keeps the live side, the Keychain and the network out", () => {
  const text = profiles({
    root: path.join(os.tmpdir(), "studio-canary-x"),
    tmp: os.tmpdir(),
    home: "/Users/owner",
    liveState: "/Users/owner/.local/state/codex-agents",
    app: path.join(os.tmpdir(), "studio-canary-x/apps/Codex Studio.app"),
    port: 4800,
    proxyPort: 4900,
    readable: ["/Users/owner/.codex/auth.json"],
  });
  for (const [name, profile] of Object.entries(text)) {
    assert.match(profile, /\(deny default\)/, name);
    assert.match(profile, /global-name "com.apple.SecurityServer"/, name);
    assert.match(
      profile,
      /\(deny file-write\* \(subpath "\/Users\/owner\/.local\/state\/codex-agents"\) \(subpath "\/Applications"\)/,
      name,
    );
    assert.match(
      profile,
      /\(deny file-write\* \(literal "\/Users\/owner\/.codex\/auth.json"\)\)/,
      name,
    );
    assert.match(
      profile,
      /\(deny process-exec \(subpath "\/Applications\/Codex Studio.app"\)/,
      name,
    );
    assert.match(profile, /\(remote tcp "localhost:4900"\)/, name);
    assert.doesNotMatch(profile, /\(allow network-outbound\)/, name);
  }
  assert.match(
    text.installer,
    /\(deny process-exec \(literal "\/usr\/bin\/osascript"\) \(literal "\/usr\/bin\/open"\)\)/,
  );
});
