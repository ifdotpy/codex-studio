#!/usr/bin/env node

import ts from "typescript";
import {
  enumLiteralDiagnostics,
  generatedTypeDiagnostics,
  switchDefaultDiagnostics,
} from "./enum-literals.ts";
import { checkRatchet, fingerprintMatches, readBaseline } from "./ratchet.ts";

import {
  JAVASCRIPT_EXCEPTIONS_FILE,
  type JavaScriptException,
  isJavaScript,
  isTypeScriptProject,
  javascriptFindings,
} from "./javascript-policy.ts";

import { execFileSync, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  realpathSync,
  rmSync,
  writeFileSync,
  mkdirSync,
  symlinkSync,
  cpSync,
  readFileSync,
  readdirSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
const defaultRoot = path.resolve(scriptDirectory, "../../../..");
const mode = process.argv[2];
const check = process.argv[3];
const repositoryRoot = defaultRoot;
const codeExtensions = new Set([
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".ts",
  ".tsx",
  ".cts",
  ".mts",
]);
const formatExtensions = new Set([
  ...codeExtensions,
  ".css",
  ".html",
  ".json",
  ".jsonc",
  ".md",
  ".yaml",
  ".yml",
]);

function git(
  args: string[],
  options: import("node:child_process").ExecFileSyncOptionsWithBufferEncoding = {},
) {
  return execFileSync("git", args, {
    cwd: repositoryRoot,
    encoding: "buffer",
    maxBuffer: 64 * 1024 * 1024,
    ...options,
  });
}

function listPaths(output: Buffer) {
  return output.toString("utf8").split("\0").filter(Boolean);
}

function isCheckable(relativePath: string, extensions: ReadonlySet<string>) {
  return extensions.has(path.extname(relativePath).toLowerCase());
}

function getPaths() {
  return listPaths(
    git(["diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR"]),
  );
}

function getIndexPaths() {
  return new Set(listPaths(git(["ls-files", "-z"])));
}

function run(binary: string, args: string[], cwd: string) {
  const result = spawnSync(binary, args, { cwd, stdio: "inherit" });
  if (result.error) {
    throw result.error;
  }
  return result.status ?? 1;
}

const enumOwners = [
  "workspaces/client/apps/web/src/generated/api.ts",
  "workspaces/client/apps/web/src/generated/providerKind.ts",
];
function enumTarget(file: string) {
  return (
    file.startsWith("workspaces/client/apps/web/src/") &&
    /\.tsx?$/.test(file) &&
    !file.includes("/generated/") &&
    !file.endsWith(".generated.ts")
  );
}
function toolingTarget(file: string) {
  return (
    file.startsWith("workspaces/tooling/") &&
    /\.tsx?$/.test(file) &&
    !file.endsWith(".d.ts")
  );
}
function otherClientTarget(file: string) {
  return (
    (file.startsWith("workspaces/client/apps/desktop/") ||
      file.startsWith("workspaces/client/apps/native/")) &&
    /\.[cm]?tsx?$/.test(file) &&
    !file.endsWith(".d.ts") &&
    !file.includes("/generated/") &&
    !file.endsWith(".generated.ts")
  );
}

function checkTypeGuards(
  root: string,
  allPaths: Iterable<string>,
  targets: string[],
  partial: boolean,
  json = false,
): number {
  const paths = [...allPaths];
  const owners = paths.filter(
    (file) =>
      file.endsWith(".ts") &&
      (file.startsWith("workspaces/client/apps/web/src/generated/") ||
        (file.startsWith("workspaces/client/apps/web/src/") &&
          file.endsWith(".generated.ts")) ||
        (file.startsWith("workspaces/providers/") &&
          (file.includes("/codex-generated/") ||
            file.endsWith(".generated.ts")))) &&
      !file.endsWith(".type-test.ts"),
  );
  const web = targets.filter(enumTarget);
  const tooling = targets.filter(toolingTarget);
  const desktop = targets.filter(
    (file) =>
      otherClientTarget(file) &&
      file.startsWith("workspaces/client/apps/desktop/"),
  );
  const native = targets.filter(
    (file) =>
      otherClientTarget(file) &&
      file.startsWith("workspaces/client/apps/native/"),
  );
  const matches: Record<
    "generated-types" | "switch-default",
    { file: string; text: string; message: string }[]
  > = { "generated-types": [], "switch-default": [] };
  for (const [selected, configFile] of [
    [web, "workspaces/client/apps/web/tsconfig.json"],
    [desktop, "workspaces/client/apps/desktop/tsconfig.json"],
    [native, "workspaces/client/apps/native/tsconfig.json"],
    [tooling, "workspaces/tooling/tsconfig.json"],
  ] as const) {
    if (!selected.length) continue;
    const config = ts.getParsedCommandLineOfConfigFile(
      path.join(root, configFile),
      {},
      {
        ...ts.sys,
        onUnRecoverableConfigFileDiagnostic(diagnostic) {
          throw new Error(
            ts.flattenDiagnosticMessageText(diagnostic.messageText, " "),
          );
        },
      },
    );
    if (!config || config.errors.length)
      throw new Error(`Cannot parse ${configFile}`);
    const absoluteTargets = selected.map((file) => path.join(root, file));
    const absoluteOwners = owners.map((file) => path.join(root, file));
    const program = ts.createProgram(
      [
        ...absoluteTargets,
        ...(configFile.includes("/client/") ? absoluteOwners : []),
      ],
      config.options,
    );
    const relative = (finding: {
      file: string;
      text: string;
      message: string;
    }) => ({
      ...finding,
      file: path.relative(root, finding.file).replaceAll(path.sep, "/"),
    });
    if (configFile.includes("/client/"))
      matches["generated-types"].push(
        ...generatedTypeDiagnostics(
          program,
          absoluteTargets,
          absoluteOwners,
        ).map(relative),
      );
    matches["switch-default"].push(
      ...switchDefaultDiagnostics(program, absoluteTargets).map(relative),
    );
  }
  const violations = Object.fromEntries(
    Object.entries(matches).map(([rule, entries]) => [
      rule,
      fingerprintMatches(entries),
    ]),
  );
  if (json) {
    console.log(JSON.stringify(violations, null, 2));
    return 0;
  }
  let failed = false;
  const baseline = readBaseline(
    path.join(
      root,
      "workspaces/tooling/apps/repository-checks/ratchet-baseline.json",
    ),
  );
  if (partial) {
    const selected = new Set(targets);
    for (const rule of Object.keys(baseline))
      baseline[rule] =
        baseline[rule]?.filter((entry) => selected.has(entry.file)) ?? [];
  }
  for (const [rule, entries] of Object.entries(violations)) {
    const diagnostics = checkRatchet(rule, entries, baseline);
    for (const message of diagnostics) console.error(message);
    failed ||= diagnostics.length > 0;
  }
  return failed ? 1 : 0;
}
function checkEnums(
  root: string,
  relativeTargets: string[],
  json: boolean,
  staged = false,
): number {
  const configPath = path.join(
    root,
    "workspaces/client/apps/web/tsconfig.json",
  );
  const config = ts.getParsedCommandLineOfConfigFile(
    configPath,
    {},
    {
      ...ts.sys,
      onUnRecoverableConfigFileDiagnostic(diagnostic) {
        throw new Error(
          ts.flattenDiagnosticMessageText(diagnostic.messageText, " "),
        );
      },
    },
  );
  if (!config || config.errors.length)
    throw new Error("Cannot parse web TypeScript configuration");
  const targets = relativeTargets.map((file) => path.join(root, file));
  const owners = enumOwners.map((file) => path.join(root, file));
  const options = { ...config.options, noUncheckedSideEffectImports: true };
  const host = ts.createCompilerHost(options);
  const wasmFacade = path.join(
    root,
    "workspaces/client/apps/web/src/wasm/client.ts",
  );
  const wasmDeclaration = path.join(
    root,
    "workspaces/client/apps/web/src/wasm/generated/client.d.ts",
  );
  if (
    staged &&
    !existsSync(wasmDeclaration) &&
    existsSync(wasmFacade) &&
    !targets.includes(wasmFacade)
  ) {
    // The checked-in facade explicitly owns its public signatures; derive a
    // declaration without building or inventing a type for the generated binding.
    const result = ts.transpileDeclaration(readFileSync(wasmFacade, "utf8"), {
      fileName: wasmFacade,
      reportDiagnostics: true,
      compilerOptions: { isolatedDeclarations: true },
    });
    if (result.diagnostics?.length)
      throw new Error(
        "WASM facade requires explicit exported types for staged enum checks",
      );
    const original = host.getSourceFile;
    host.getSourceFile = (
      file,
      languageVersion,
      onError,
      shouldCreateNewSourceFile,
    ) =>
      file === wasmFacade
        ? ts.createSourceFile(file, result.outputText, languageVersion, true)
        : original(file, languageVersion, onError, shouldCreateNewSourceFile);
  }
  const declarations = config.fileNames.filter((file) =>
    file.endsWith(".d.ts"),
  );
  const program = ts.createProgram(
    [...targets, ...owners, ...declarations],
    options,
    host,
  );
  const missing = ts
    .getPreEmitDiagnostics(program)
    .filter((diagnostic) =>
      [2305, 2307, 2688, 2724, 2792, 7016].includes(diagnostic.code),
    );
  if (missing.length)
    throw new Error(
      `Cannot resolve TypeScript dependencies: ${ts.formatDiagnosticsWithColorAndContext(missing, { getCurrentDirectory: () => root, getCanonicalFileName: (file) => file, getNewLine: () => "\n" })}`,
    );
  const findings = enumLiteralDiagnostics(program, targets, owners).map(
    (finding) => ({
      ...finding,
      file: path.relative(root, finding.file),
      ownerFile: path.relative(root, finding.ownerFile),
    }),
  );
  if (json) console.log(JSON.stringify(findings, null, 2));
  else
    for (const finding of findings)
      console.error(
        `Enum policy: ${finding.file}:${finding.line}:${finding.column}: import and use ${finding.owner}[${JSON.stringify(finding.member)}] from ${finding.ownerFile} instead of ${JSON.stringify(finding.literal)}`,
      );
  return findings.length ? 1 : 0;
}

function main() {
  if (mode === "lint-all") {
    const args = process.argv.slice(3).filter((arg) => arg !== "--");
    const status = run(
      path.join(repositoryRoot, "node_modules", ".bin", "oxlint"),
      ["--deny-warnings", ...args],
      repositoryRoot,
    );
    if (status !== 0) return status;
    const paths = args
      .filter((arg) => !arg.startsWith("-"))
      .map((arg) =>
        path
          .relative(repositoryRoot, path.resolve(repositoryRoot, arg))
          .replaceAll(path.sep, "/"),
      );
    const targets = [...getIndexPaths()]
      .filter(enumTarget)
      .filter(
        (file) =>
          !paths.length ||
          paths.includes("") ||
          paths.some(
            (selected) => file === selected || file.startsWith(`${selected}/`),
          ),
      );
    const allPaths = [...getIndexPaths()];
    const selected = allPaths
      .filter(
        (file) =>
          enumTarget(file) || otherClientTarget(file) || toolingTarget(file),
      )
      .filter(
        (file) =>
          !paths.length ||
          paths.includes("") ||
          paths.some(
            (selected) => file === selected || file.startsWith(`${selected}/`),
          ),
      );
    const statusTypes = checkTypeGuards(
      repositoryRoot,
      allPaths,
      selected,
      paths.length > 0 && !paths.includes(""),
    );
    if (statusTypes) return statusTypes;
    return targets.length ? checkEnums(repositoryRoot, targets, false) : 0;
  }
  if (mode === "type-guards" && check === "--json") {
    const paths = [...getIndexPaths()];
    return checkTypeGuards(
      repositoryRoot,
      paths,
      paths.filter(
        (file) =>
          enumTarget(file) || otherClientTarget(file) || toolingTarget(file),
      ),
      false,
      true,
    );
  }
  if (mode === "enums" && (check === undefined || check === "--json")) {
    return checkEnums(
      repositoryRoot,
      [...getIndexPaths()].filter(enumTarget),
      check === "--json",
    );
  }
  if (
    mode !== "staged" ||
    (check !== undefined && check !== "lint" && check !== "format")
  ) {
    throw new Error(
      "Usage: node workspaces/tooling/apps/repository-checks/check-code.ts staged [lint|format] | enums [--json]",
    );
  }

  const selectedPaths = getPaths();
  const changedPaths = listPaths(
    git(["diff", "--cached", "--name-only", "-z"]),
  );
  const projectsChanged = changedPaths.some(
    (file) =>
      isTypeScriptProject(file) ||
      file === "tsconfig.base.json" ||
      file === JAVASCRIPT_EXCEPTIONS_FILE,
  );
  const lintPaths = selectedPaths.filter((relativePath) =>
    isCheckable(relativePath, codeExtensions),
  );
  const formatPaths = selectedPaths.filter((relativePath) =>
    isCheckable(relativePath, formatExtensions),
  );
  if (check === "lint") {
    formatPaths.length = 0;
  } else if (check === "format") {
    lintPaths.length = 0;
  }

  if (lintPaths.length === 0 && formatPaths.length === 0 && !projectsChanged) {
    console.log("No staged source or supported text files to check.");
    return 0;
  }

  // The real path: on macOS the temporary folder is below the /var link, and Oxfmt matches
  // ignorePatterns only when a file path starts with the folder of its configuration.
  const temporaryRoot = realpathSync(
    mkdtempSync(path.join(os.tmpdir(), "codex-staged-check-")),
  );
  try {
    const indexPaths = getIndexPaths();
    const exceptions: JavaScriptException[] = JSON.parse(
      git(["cat-file", "blob", ":" + JAVASCRIPT_EXCEPTIONS_FILE]).toString(
        "utf8",
      ),
    );
    const enumTargets = check !== "format" ? lintPaths.filter(enumTarget) : [];
    const typeTargets =
      check !== "format"
        ? lintPaths.filter(
            (file) =>
              enumTarget(file) ||
              otherClientTarget(file) ||
              toolingTarget(file),
          )
        : [];
    const enumSources = typeTargets.length
      ? [...indexPaths].filter((file) => /\.(?:[cm]?[jt]sx?|json)$/.test(file))
      : [];
    const policyPaths = [
      ...new Set([
        ...selectedPaths.filter(isJavaScript),
        ...(projectsChanged ? [...indexPaths].filter(isJavaScript) : []),
      ]),
    ];
    for (const relativePath of new Set([
      ...lintPaths,
      ...formatPaths,
      ...policyPaths,
      ...enumSources,
      ...[...indexPaths].filter(
        (file) => isTypeScriptProject(file) || file === "tsconfig.base.json",
      ),
    ])) {
      const destination = path.join(temporaryRoot, relativePath);
      mkdirSync(path.dirname(destination), { recursive: true });
      const content = git(["cat-file", "blob", `:${relativePath}`]);
      writeFileSync(destination, content);
    }
    for (const relativePath of [".oxlintrc.json", ".oxfmtrc.json"]) {
      if (!indexPaths.has(relativePath)) continue;
      writeFileSync(
        path.join(temporaryRoot, relativePath),
        git(["cat-file", "blob", `:${relativePath}`]),
      );
    }

    const policyFindings = javascriptFindings(
      temporaryRoot,
      [...indexPaths],
      policyPaths,
      exceptions,
    );
    if (policyFindings.length) {
      for (const finding of policyFindings)
        console.error(`JavaScript policy: ${finding}`);
      return 1;
    }

    if (typeTargets.length) {
      // Generated WASM declarations are build outputs, not Git-index source.
      const wasmOutput = "workspaces/client/apps/web/src/wasm/generated";
      if (existsSync(path.join(repositoryRoot, wasmOutput))) {
        mkdirSync(path.dirname(path.join(temporaryRoot, wasmOutput)), {
          recursive: true,
        });
        cpSync(
          path.join(repositoryRoot, wasmOutput),
          path.join(temporaryRoot, wasmOutput),
          { recursive: true },
        );
      }
      const packageDirectories = [
        ...new Set([
          "",
          ...[...indexPaths]
            .filter((file) => path.basename(file) === "package.json")
            .map((file) =>
              path.dirname(file) === "." ? "" : path.dirname(file),
            ),
        ]),
      ];
      const workspacePackages = new Map(
        packageDirectories.map((directory) => [
          path.resolve(repositoryRoot, directory),
          path.resolve(temporaryRoot, directory),
        ]),
      );
      function linkEntry(source: string, destination: string) {
        let actual: string;
        try {
          actual = realpathSync(source);
        } catch {
          return;
        }
        symlinkSync(
          workspacePackages.get(actual) ?? source,
          destination,
          "dir",
        );
      }
      for (const directory of packageDirectories) {
        const source = path.join(repositoryRoot, directory, "node_modules");
        const destination = path.join(temporaryRoot, directory, "node_modules");
        if (!existsSync(source)) continue;
        mkdirSync(destination, { recursive: true });
        for (const entry of readdirSync(source)) {
          if (entry.startsWith(".")) continue;
          const from = path.join(source, entry);
          const to = path.join(destination, entry);
          if (entry.startsWith("@")) {
            mkdirSync(to, { recursive: true });
            for (const name of readdirSync(from))
              linkEntry(path.join(from, name), path.join(to, name));
          } else linkEntry(from, to);
        }
      }
      const typeStatus = checkTypeGuards(
        temporaryRoot,
        indexPaths,
        typeTargets,
        true,
      );
      if (typeStatus) return typeStatus;
      const status = enumTargets.length
        ? checkEnums(temporaryRoot, enumTargets, false, true)
        : 0;
      if (status !== 0) return status;
    }

    const binDirectory = path.join(repositoryRoot, "node_modules", ".bin");
    const oxlint = path.join(binDirectory, "oxlint");
    const oxfmt = path.join(binDirectory, "oxfmt");
    if (
      (lintPaths.length > 0 && !existsSync(oxlint)) ||
      (formatPaths.length > 0 && !existsSync(oxfmt))
    ) {
      throw new Error(
        "Oxlint/Oxfmt dependencies are missing; install them with `pnpm install --frozen-lockfile` before committing.",
      );
    }

    if (lintPaths.length > 0) {
      console.log("Checking staged JavaScript/TypeScript with Oxlint...");
      const status = run(
        oxlint,
        [
          "--deny-warnings",
          "--disable-nested-config",
          ...lintPaths.map((relativePath) =>
            path.join(temporaryRoot, relativePath),
          ),
        ],
        temporaryRoot,
      );
      if (status !== 0) return status;
    }

    if (formatPaths.length > 0) {
      console.log("Checking staged supported text files with Oxfmt...");
      const status = run(
        oxfmt,
        [
          "--check",
          "--disable-nested-config",
          "--no-error-on-unmatched-pattern",
          ...formatPaths.map((relativePath) =>
            path.join(temporaryRoot, relativePath),
          ),
        ],
        temporaryRoot,
      );
      if (status !== 0) return status;
    }
    return 0;
  } finally {
    rmSync(temporaryRoot, { recursive: true, force: true });
  }
}

try {
  process.exitCode = main();
} catch (error) {
  console.error(`Pre-commit checks failed: ${(error as Error).message}`);
  process.exitCode = 1;
}
