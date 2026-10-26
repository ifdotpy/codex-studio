import assert from "node:assert/strict";
import { test } from "vitest";
import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
const { layrDesk, vaultKeysScript } = createRequire(import.meta.url)(
  "./vault-keys.cjs",
);

test("the packaged app runs its own layr-desk", () => {
  assert.equal(
    layrDesk({ packaged: true, resources: "/A/Resources/workspace", env: {} }),
    "/A/Resources/workspace/bin/layr-desk",
  );
  assert.equal(
    layrDesk({
      packaged: false,
      resources: "",
      env: { STUDIO_LAYR_DESK_BIN: "/x" },
    }),
    "/x",
  );
});

test("the script runs layr-desk keys once for a valid vault name only", async () => {
  const folder = mkdtempSync(path.join(tmpdir(), "vault-keys-"));
  try {
    const log = path.join(folder, "args");
    const program = path.join(folder, "fake desk's");
    writeFileSync(program, `#!/bin/sh\nprintf '%s\\n' "$@" > '${log}'\n`, {
      mode: 0o755,
    });
    const file = await vaultKeysScript("my-project", { folder, program });
    assert.equal(statSync(file).mode & 0o777, 0o700);
    assert.match(
      readFileSync(file, "utf8"),
      /keys --project 'my-project' --clear/,
    );
    execFileSync(file);
    assert.deepEqual(readFileSync(log, "utf8").trim().split("\n"), [
      "keys",
      "--project",
      "my-project",
      "--clear",
    ]);
    assert.throws(() => statSync(file), "the script removes itself");
    for (const bad of ["api", "-x", "a b", "a'b", "x".repeat(45), 7])
      await assert.rejects(vaultKeysScript(bad, { folder, program }));
  } finally {
    rmSync(folder, { recursive: true, force: true });
  }
});
