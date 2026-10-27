import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { copyFileSync, existsSync, readdirSync, statSync } from "node:fs";
import { once } from "node:events";
import { test } from "node:test";

import { binary, compatible, legacy, fixture } from "./install-layr-store.mts";

test("only the Layr build receives the manifest upstream revision", async (t) => {
  const f = fixture(t);
  const run = await f.run();
  assert.equal(run.status, 0, run.stderr);
  assert.equal(f.read("/build-environments"), `${"a".repeat(40)}\n\n`);
});

test("the installer upgrades legacy metadata once before binary activation", async (t) => {
  const f = fixture(t);
  const first = await f.run();
  assert.equal(first.status, 0, first.stderr);
  assert.equal(f.read(`${f.store}/calls`), "upgrade\n");
  assert.equal(f.read(`${f.store}/machine-key`), "keep-key");
  assert.equal(f.read(`${f.store}/project-data`), "keep-project");
  const receipt = f.read(`${f.state}/layr-store-upgrades/store-upgrade-1.json`);
  assert.match(receipt, /"stage":"complete"/);
  assert.match(receipt, /"requestId":"store-upgrade:1"/);
  assert.match(receipt, new RegExp(f.identity));
  const repeat = await f.run();
  assert.equal(repeat.status, 0, repeat.stderr);
  assert.equal(f.read(`${f.store}/calls`), "upgrade\n");
  assert.equal(
    f.read(`${f.state}/layr-store-upgrades/store-upgrade-1.json`),
    receipt,
  );
});

for (const report of [
  { ...legacy, upgrade: null },
  { ...legacy, upgrade: { ...legacy.upgrade, metadataOnly: false } },
  { ...legacy, upgrade: { ...legacy.upgrade, fromFormat: 9 } },
  { ...legacy, state: "incompatible", store: { ...binary, format: 9 } },
])
  test(`the installer refuses unsupported metadata: ${JSON.stringify(report)}`, async (t) => {
    const f = fixture(t, report);
    const before = f.read(`${f.store}/report.json`);
    const run = await f.run();
    assert.notEqual(run.status, 0);
    assert.match(run.stderr, /docs\/vm-layr.md/);
    assert.equal(f.read(`${f.store}/report.json`), before);
    assert(!existsSync(f.file(`${f.store}/calls`)));
    assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
    assert(!existsSync(f.file(`${f.state}/layr-build`)));
  });

test("a failed upgrade keeps the old binary and never repeats an uncertain request", async (t) => {
  const f = fixture(t);
  f.write(`${f.store}/fail`, "fail");
  const run = await f.run();
  assert.notEqual(run.status, 0);
  assert.match(run.stderr, /upgrade failed/);
  assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
  assert(!existsSync(f.file(`${f.state}/layr-build`)));
  assert.notEqual((await f.run()).status, 0);
  assert.equal(f.read(`${f.store}/calls`), "upgrade\n");
  // The marker is authoritative after a lost success or an operator repair.
  f.write(`${f.store}/report.json`, JSON.stringify(compatible));
  const recovered = await f.run();
  assert.equal(recovered.status, 0, recovered.stderr);
  assert.equal(f.read(`${f.store}/calls`), "upgrade\n");
  assert.match(
    f.read(`${f.state}/layr-store-upgrades/store-upgrade-1.json`),
    /"stage":"complete"/,
  );
});

for (const state of ["absent", "empty"])
  test(`a ${state} store needs no explicit upgrade`, async (t) => {
    const f = fixture(t, {
      binary,
      store: null,
      compatible: true,
      state,
      upgrade: null,
    });
    const run = await f.run();
    assert.equal(run.status, 0, run.stderr);
    assert(!existsSync(f.file(`${f.store}/calls`)));
    assert(
      !existsSync(
        f.file(`${f.state}/layr-store-upgrades/store-upgrade-1.json`),
      ),
    );
  });

test("a lost upgrade response reconciles the marker without a second command", async (t) => {
  const f = fixture(t);
  f.write(`${f.store}/lost`, "lost");
  const first = await f.run();
  assert.notEqual(first.status, 0);
  assert.match(first.stderr, /lost response/);
  assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
  assert.match(
    f.read(`${f.state}/layr-store-upgrades/store-upgrade-1.json`),
    /"stage":"pending"/,
  );
  const repeat = await f.run();
  assert.equal(repeat.status, 0, repeat.stderr);
  assert.equal(f.read(`${f.store}/calls`), "upgrade\n");
  assert.match(
    f.read(`${f.state}/layr-store-upgrades/store-upgrade-1.json`),
    /"stage":"complete"/,
  );
});

test("a successful command without a compatible marker cannot activate the binary", async (t) => {
  const f = fixture(t);
  f.write(`${f.store}/after.json`, JSON.stringify(legacy));
  const run = await f.run();
  assert.notEqual(run.status, 0);
  assert.match(run.stderr, /did not establish a compatible format/);
  assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
});

test("an invalid report cannot activate the binary", async (t) => {
  const f = fixture(t, {
    binary,
    store: null,
    compatible: true,
    state: "unexpected",
  });
  const run = await f.run();
  assert.notEqual(run.status, 0);
  assert.match(run.stderr, /docs\/vm-layr.md/);
  assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
});

test("a deferred install keeps the old client until coordinated activation", async (t) => {
  const f = fixture(t);
  const staged = await f.run({ defer: true });
  assert.equal(staged.status, 0, staged.stderr);
  assert.equal(
    execFileSync(f.file("/usr/local/bin/layr"), { encoding: "utf8" }).trim(),
    "old-binary",
  );
  assert(!existsSync(f.file(`${f.store}/calls`)));
  assert(!existsSync(f.file(`${f.state}/layr-build`)));
  assert(!f.read("systemctl.log").includes("stop codex-studio-layr.service"));
  const activated = await f.run({ activate: true });
  assert.equal(activated.status, 0, activated.stderr);
  assert.equal(f.read(`${f.store}/calls`), "upgrade\n");
  assert(!f.read("/usr/local/bin/layr").includes("old-binary"));
  const calls = f.read("systemctl.log");
  assert(
    calls.indexOf("stop codex-studio-layr.service") <
      calls.indexOf("start codex-studio-layr.service"),
  );
});

for (const failure of ["fail-start", "bad-hello"])
  test(`activation rolls back after ${failure}`, async (t) => {
    const f = fixture(t);
    const previousBuild = "f".repeat(64) + "\n";
    f.write(`${f.state}/layr-build`, previousBuild);
    assert.equal((await f.run({ defer: true })).status, 0);
    f.write(`/${failure}`, "fail");
    const result = await f.run({ activate: true });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /docs\/vm-layr.md/);
    assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
    assert.equal(f.read(`${f.state}/layr-build`), previousBuild);
    assert.match(
      f.read("systemctl.log"),
      /start codex-studio-layr.service codex-studio-layr-admin.service/,
    );
  });

test("activation, not a broker restart, completes the update receipt", async (t) => {
  const f = fixture(t);
  assert.equal((await f.run({ defer: true })).status, 0);
  f.write(
    "/update-status.json",
    JSON.stringify({
      operationId: "fixture",
      stage: "restart_scheduled",
      layrActivation: true,
    }),
  );
  const result = await f.run({
    activate: true,
    status: f.file("update-status.json"),
  });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(f.read("/update-status.json")).stage, "complete");
});

test("a failed activation saves the update error after it restores the old binary", async (t) => {
  const f = fixture(t);
  assert.equal((await f.run({ defer: true })).status, 0);
  f.write(
    "/update-status.json",
    JSON.stringify({
      operationId: "fixture",
      stage: "restart_scheduled",
      layrActivation: true,
    }),
  );
  f.write("/bad-hello", "fail");
  const result = await f.run({
    activate: true,
    status: f.file("update-status.json"),
  });
  assert.notEqual(result.status, 0);
  assert.equal(
    JSON.parse(f.read("/update-status.json")).stage,
    "restart_failed",
  );
  assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
});

test("a second install cannot change a staged request's binary identity", async (t) => {
  const f = fixture(t);
  assert.equal((await f.run({ defer: true })).status, 0);
  const pending = f.read(`${f.state}/layr-install-pending.json`);
  f.write("/candidate", f.read("/candidate") + "\n# different binary\n");
  const result = await f.run({ defer: true });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /different Layr install is pending/);
  assert.equal(f.read(`${f.state}/layr-install-pending.json`), pending);
  assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
});

test("a changed staged binary refuses before the daemon stops", async (t) => {
  const f = fixture(t);
  assert.equal((await f.run({ defer: true })).status, 0);
  f.write(
    "/usr/local/bin/layr.next",
    f.read("/usr/local/bin/layr.next") + "\n# changed\n",
  );
  const result = await f.run({ activate: true });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /differs from its receipt/);
  assert(!f.read("systemctl.log").includes("stop codex-studio-layr.service"));
});

const realBinary = process.env.STUDIO_TEST_REAL_LAYR;

test(
  "the published guard adopts a legacy fixture once",
  { skip: !realBinary },
  async (t) => {
    const f = fixture(t);
    assert(realBinary);
    copyFileSync(realBinary, f.file("candidate"));
    const result = await f.run();
    assert.equal(result.status, 0, result.stderr);
    const marker = `${f.store}/store-version.json`;
    const first = statSync(f.file(marker), { bigint: true }).mtimeNs;
    assert.equal(JSON.parse(f.read(marker)).format, binary.format);
    const receipt = f.read(
      `${f.state}/layr-store-upgrades/store-upgrade-1.json`,
    );
    assert.equal((await f.run()).status, 0);
    assert.equal(statSync(f.file(marker), { bigint: true }).mtimeNs, first);
    assert.equal(
      f.read(`${f.state}/layr-store-upgrades/store-upgrade-1.json`),
      receipt,
    );
    assert.equal(f.read(`${f.store}/machine-key`), "keep-key");
    assert.equal(f.read(`${f.store}/project-data`), "keep-project");
  },
);

test(
  "the published guard refuses a newer store without changing it",
  { skip: !realBinary },
  async (t) => {
    const f = fixture(t);
    assert(realBinary);
    copyFileSync(realBinary, f.file("candidate"));
    f.write(
      `${f.store}/store-version.json`,
      JSON.stringify({ ...binary, format: 99 }),
    );
    const before = readdirSync(f.file(f.store)).sort();
    const marker = f.read(`${f.store}/store-version.json`);
    const result = await f.run();
    assert.notEqual(result.status, 0);
    assert.deepEqual(readdirSync(f.file(f.store)).sort(), before);
    assert.equal(f.read(`${f.store}/store-version.json`), marker);
    assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
  },
);

test(
  "the published guard refuses an in-use store and keeps the old binary",
  { skip: !realBinary || process.platform !== "linux" },
  async (t) => {
    const f = fixture(t);
    assert(realBinary);
    copyFileSync(realBinary, f.file("candidate"));
    const holder = spawn(
      "flock",
      [f.file(`${f.store}/.layr-version.lock`), "sh", "-c", "printf held; cat"],
      { stdio: ["pipe", "pipe", "pipe"] },
    );
    const exit = once(holder, "exit");
    t.after(async () => {
      holder.stdin.end();
      await exit;
    });
    await once(holder.stdout, "data");
    const result = await f.run();
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /store is in use/);
    assert(!existsSync(f.file(`${f.store}/store-version.json`)));
    assert.match(f.read("/usr/local/bin/layr"), /old-binary/);
    assert.match(
      f.read("systemctl.log"),
      /start codex-studio-layr.service codex-studio-layr-admin.service/,
    );
  },
);

test("an unchanged source cannot activate a different pending update", async (t) => {
  const f = fixture(t);
  assert.equal((await f.run({ defer: true })).status, 0);
  const pending = JSON.parse(f.read(`${f.state}/layr-install-pending.json`));
  pending.sourceIdentity = "f".repeat(64);
  f.write(`${f.state}/layr-install-pending.json`, JSON.stringify(pending));
  f.write("/usr/local/bin/layr", f.read("/usr/local/bin/layr.next"));
  f.write(`${f.store}/report.json`, JSON.stringify(compatible));
  f.write(`${f.state}/layr-build`, f.identity + "\n");
  const result = await f.run({ defer: true });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /different Layr install is pending/);
  assert(!f.read("systemctl.log").includes("stop codex-studio-layr.service"));
});
