import test from "node:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  buildReleaseBinaries,
  manifestPath,
  prebuiltReleaseDirectory,
  releaseArtifacts,
  releaseTargetEnvironment,
  verifyReleaseBinaries,
  writeReleaseManifest,
} from "../../../../client/apps/desktop/release-binaries.ts";
import {
  parseFlowArguments,
  planFlow,
} from "../../../../client/apps/native/package-flow.ts";

const commitA = "a".repeat(40);
const commitB = "b".repeat(40);

test("desktop packaging uses the manifest-owned Electron version", async () => {
  const source = await readFile(
    new URL("../../../../client/apps/desktop/package.ts", import.meta.url),
    "utf8",
  );
  assert.doesNotMatch(
    source,
    /\belectronVersion\s*:/,
    "@electron/packager must infer Electron from desktop/package.json; an explicit override can ship a different runtime than the installed dependency",
  );
});

async function fakeRelease(commit = commitA) {
  const directory = await mkdtemp(path.join(os.tmpdir(), "release-binaries-"));
  await mkdir(path.join(directory, "release"));
  for (const name of releaseArtifacts())
    await writeFile(path.join(directory, "release", name), `binary ${name}`);
  await writeReleaseManifest({ directory, commit });
  return directory;
}

async function withRelease(
  callback: (directory: string) => Promise<void>,
  commit?: string,
) {
  const directory = await fakeRelease(commit);
  try {
    await callback(directory);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("accepts binaries built from the same commit", () =>
  withRelease(async (directory) => {
    const manifest = await verifyReleaseBinaries({
      directory,
      commit: commitA,
    });
    assert.equal(manifest.commit, commitA);
    assert.deepEqual(
      Object.keys(manifest.files).sort(),
      [...releaseArtifacts()].sort(),
    );
  }));

test("names the shipped release artifacts for each Unix platform", () => {
  assert.deepEqual(releaseArtifacts("darwin"), [
    "studio-runtime-server",
    "codex-process-supervisor",
    "codex-diagnostics",
    "studio-peer-link",
  ]);
  assert.deepEqual(releaseArtifacts("linux"), [
    "studio-runtime-server",
    "codex-process-supervisor",
    "codex-diagnostics",
    "studio-peer-link",
  ]);
});

test("refuses binaries built from another commit", () =>
  withRelease(async (directory) => {
    await assert.rejects(
      verifyReleaseBinaries({ directory, commit: commitB }),
      new RegExp(`built from commit ${commitA}, not ${commitB}`),
    );
  }));

test("refuses a binary that changed after the build", () =>
  withRelease(async (directory) => {
    const [artifact] = releaseArtifacts();
    assert.ok(artifact);
    await writeFile(
      path.join(directory, "release", artifact),
      "rebuilt elsewhere",
    );
    await assert.rejects(
      verifyReleaseBinaries({ directory, commit: commitA }),
      /SHA-256 of .* differs from the manifest/,
    );
  }));

test("refuses another platform, a missing binary and a missing manifest", () =>
  withRelease(async (directory) => {
    await assert.rejects(
      verifyReleaseBinaries({
        directory,
        commit: commitA,
        arch: "other",
      }),
      /were built for/,
    );
    const [, artifact] = releaseArtifacts();
    assert.ok(artifact);
    await rm(path.join(directory, "release", artifact));
    await assert.rejects(
      verifyReleaseBinaries({ directory, commit: commitA }),
      /are missing/,
    );
    await rm(manifestPath(directory));
    await assert.rejects(
      verifyReleaseBinaries({ directory, commit: commitA }),
      /have no manifest/,
    );
  }));

test("refuses a manifest that omits a required artifact", () =>
  withRelease(async (directory) => {
    const manifest: Awaited<ReturnType<typeof writeReleaseManifest>> =
      JSON.parse(await readFile(manifestPath(directory), "utf8"));
    const [artifact] = releaseArtifacts();
    assert.ok(artifact);
    delete manifest.files[artifact];
    await writeFile(manifestPath(directory), JSON.stringify(manifest));
    await assert.rejects(
      verifyReleaseBinaries({ directory, commit: commitA }),
      /the packager needs/,
    );
  }));

test("a packager uses no prebuilt directory unless one is selected", async () => {
  assert.equal(
    await prebuiltReleaseDirectory("/repo", {}, () => assert.fail()),
    null,
  );
});

test("a packager refuses another commit and a dirty checkout", () =>
  withRelease(async (directory) => {
    const env = { [releaseTargetEnvironment]: directory };
    assert.equal(
      await prebuiltReleaseDirectory("/repo", env, () => ({
        commit: commitA,
        dirty: false,
      })),
      directory,
    );
    await assert.rejects(
      prebuiltReleaseDirectory("/repo", env, () => ({
        commit: commitB,
        dirty: false,
      })),
      /built from commit/,
    );
    await assert.rejects(
      prebuiltReleaseDirectory("/repo", env, () => ({
        commit: commitA,
        dirty: true,
      })),
      /differ from HEAD/,
    );
  }));

test("a build removes the old manifest first and records the built commit", async () => {
  const directory = await fakeRelease(commitB);
  try {
    const calls: {
      command: string;
      args: string[];
      target: string | undefined;
    }[] = [];
    const manifest = await buildReleaseBinaries({
      repository: "/repo",
      directory,
      state: () => ({ commit: commitA, dirty: false }),
      run: (command, args, options) => {
        calls.push({ command, args, target: options.env!.CARGO_TARGET_DIR });
        return verifyReleaseBinaries({ directory, commit: commitB }).then(
          () => assert.fail("the old manifest must be gone during the build"),
          (error: unknown) =>
            assert.match((error as Error).message, /have no manifest/),
        );
      },
    });
    assert.equal(manifest.commit, commitA);
    const [call] = calls;
    assert.ok(call);
    assert.equal(call.command, "cargo");
    assert.ok(call.args.includes("--locked"));
    assert.ok(call.args.includes("--release"));
    assert.equal(call.target, directory);
    await verifyReleaseBinaries({ directory, commit: commitA });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("a build refuses a dirty checkout and a commit that moved", async () => {
  const directory = await fakeRelease();
  try {
    await assert.rejects(
      buildReleaseBinaries({
        repository: "/repo",
        directory,
        state: () => ({ commit: commitA, dirty: true }),
        run: () => assert.fail("no build"),
      }),
      /differ from HEAD/,
    );
    const states = [
      { commit: commitA, dirty: false },
      { commit: commitB, dirty: false },
    ];
    await assert.rejects(
      buildReleaseBinaries({
        repository: "/repo",
        directory,
        state: () => states.shift()!,
        run: () => {},
      }),
      /changed during the build/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("the package flow runs the dev-profile host check unless CI is stated", () => {
  const { out, hostedCiPassed } = parseFlowArguments(["/tmp/out"]);
  assert.equal(hostedCiPassed, false);
  const plan = (skip: boolean) =>
    planFlow({ out, hostedCiPassed: skip, directory: "/r", commit: commitA });
  assert.deepEqual(
    plan(false).map((step) => step.label),
    [
      "signing check",
      "native host check",
      "release binaries",
      "package",
      "check package",
      "check package start",
    ],
  );
  const [signing, native] = plan(false);
  const signingScript = signing?.args?.[0];
  assert.ok(signingScript);
  assert.match(signingScript, /desktop\/signing-check\.ts$/);
  assert.equal(native?.args?.[0], "test-native-webview");
  const [, skipped] = plan(true);
  assert.ok(skipped);
  assert.equal(skipped.command, undefined);
  assert.match(skipped!.skipped!, new RegExp(commitA));
  assert.equal(plan(true).length, 6);
  assert.equal(
    plan(false).some(
      (step) =>
        (step as typeof step & { resolvePython?: boolean }).resolvePython ||
        (step as typeof step & { needsPython?: boolean }).needsPython ||
        step.command === "python3",
    ),
    false,
  );
  const packageStart = plan(false).at(-1)?.args?.[0];
  assert.ok(packageStart);
  assert.match(packageStart, /check-package-start\.ts$/);
  assert.equal(
    parseFlowArguments(["/tmp/out", "--hosted-ci-passed"]).hostedCiPassed,
    true,
  );
  assert.throws(
    () => parseFlowArguments(["/tmp/out", "--skip"]),
    /Unknown flag/,
  );
  assert.throws(() => parseFlowArguments([]), /Usage/);
});
