import { build } from "vite";
import ts from "typescript";
import { realpathSync, readFileSync, existsSync } from "node:fs";
import { readdir, readFile, lstat, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import {
  dirname,
  isAbsolute,
  relative,
  resolve as resolvePath,
} from "node:path";

const GLOBAL_CAPABILITIES = new Map([
  ...["localStorage", "sessionStorage", "indexedDB", "caches"].map((name) => [
    name,
    "browser-persistence",
  ]),
  ...[
    "fetch",
    "XMLHttpRequest",
    "WebSocket",
    "EventSource",
    "RTCPeerConnection",
    "BroadcastChannel",
  ].map((name) => [name, "browser-network"]),
  ["crypto", "browser-crypto"],
  ...[
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "requestIdleCallback",
    "cancelIdleCallback",
    "requestAnimationFrame",
    "cancelAnimationFrame",
  ].map((name) => [name, "business-timer"]),
  ...["Worker", "SharedWorker", "ServiceWorker"].map((name) => [
    name,
    "browser-worker",
  ]),
  ...["eval", "Function"].map((name) => [name, "runtime-code"]),
  ...["FileReader", "FileReaderSync", "Blob", "File"].map((name) => [
    name,
    "browser-bytes",
  ]),
  ...[
    "MediaRecorder",
    "AudioContext",
    "OfflineAudioContext",
    "webkitAudioContext",
    "webkitOfflineAudioContext",
  ].map((name) => [name, "browser-audio"]),
  ["Notification", "browser-notification"],
  ["open", "browser-window"],
]);
// Named navigator ports own I/O. Other navigator fields can supply UI metadata.
const NAVIGATOR_CAPABILITIES = new Map([
  ["sendBeacon", "browser-network"],
  ...["serviceWorker", "storage", "locks"].map((name) => [
    name,
    "browser-persistence",
  ]),
  ["clipboard", "browser-clipboard"],
  ...[
    "mediaDevices",
    "geolocation",
    "bluetooth",
    "usb",
    "serial",
    "hid",
    "requestMIDIAccess",
    "getUserMedia",
  ].map((name) => [name, "browser-device"]),
]);
// URL parsing is UI data work. Object URLs allocate browser-owned byte storage.
const OBJECT_URL_MEMBERS = new Set(["createObjectURL", "revokeObjectURL"]);
const GLOBAL_NAMESPACES = new Set(["navigator", "document", "URL"]);
const BROWSER_PACKAGES = new Set([
  "rxdb",
  "dexie",
  "localforage",
  "idb",
  "idb-keyval",
]);
const READONLY_STORAGE_MEMBERS = new Set(["getItem", "key", "length"]);

function cleanId(id) {
  return id.split("?")[0];
}

function canonical(id) {
  const path = cleanId(id);
  if (!isAbsolute(path)) return path;
  try {
    return realpathSync(path);
  } catch {
    return resolvePath(path);
  }
}

function exceptionPaths(root, entries, kind) {
  return new Set(
    entries.map(({ path, reason }) => {
      if (typeof path !== "string" || /[*?]/.test(path) || !reason?.trim())
        throw new Error(
          `${kind} exceptions require an exact file path and a reason.`,
        );
      return canonical(resolvePath(root, path));
    }),
  );
}

function memberName(node) {
  if (ts.isPropertyAccessExpression(node)) return node.name.text;
  if (
    ts.isElementAccessExpression(node) &&
    ts.isStringLiteralLike(node.argumentExpression)
  )
    return node.argumentExpression.text;
  return null;
}

function bindingName(element) {
  const name = element.propertyName ?? element.name;
  if (ts.isIdentifier(name) || ts.isStringLiteralLike(name)) return name.text;
  if (
    ts.isComputedPropertyName(name) &&
    ts.isStringLiteralLike(name.expression)
  )
    return name.expression.text;
  return null;
}

function unwrap(node) {
  while (
    ts.isParenthesizedExpression(node) ||
    ts.isAsExpression(node) ||
    ts.isTypeAssertionExpression(node) ||
    ts.isNonNullExpression(node)
  )
    node = node.expression;
  return node;
}

export function inspectCapabilities(code, id, permissions) {
  return inspectSourceCapabilities(code, id, permissions, ts.ScriptKind.JS);
}

function inspectSourceCapabilities(code, id, permissions, kind) {
  // The final module is JavaScript even when Vite resolves JSON or a virtual ID.
  const astId = resolvePath(
    dirname(id),
    kind === ts.ScriptKind.TSX
      ? "__studio_native_import_graph__.tsx"
      : kind === ts.ScriptKind.TS
        ? "__studio_native_import_graph__.ts"
        : kind === ts.ScriptKind.JSX
          ? "__studio_native_import_graph__.jsx"
          : kind === ts.ScriptKind.JSON
            ? "__studio_native_import_graph__.json"
            : "__studio_native_import_graph__.js",
  );
  const source = ts.createSourceFile(
    astId,
    code,
    ts.ScriptTarget.Latest,
    true,
    kind,
  );
  const host = {
    getSourceFile: (name) => (name === astId ? source : undefined),
    getDefaultLibFileName: () => "",
    writeFile: () => {},
    getCurrentDirectory: () => dirname(astId),
    getDirectories: () => [],
    fileExists: (name) => name === astId,
    readFile: (name) => (name === astId ? code : undefined),
    getCanonicalFileName: (name) => name,
    useCaseSensitiveFileNames: () => true,
    getNewLine: () => "\n",
  };
  const program = ts.createProgram(
    [astId],
    { noLib: true, noResolve: true, allowJs: true },
    host,
  );
  const checker = program.getTypeChecker();
  const findings = [];
  if (source.parseDiagnostics.length)
    return [
      {
        capability: "unparsed-module",
        detail: "The pinned AST parser cannot inspect this module.",
        line: 1,
        column: 1,
      },
    ];
  const symbolAt = (node) =>
    ts.isShorthandPropertyAssignment(node.parent) && node.parent.name === node
      ? checker.getShorthandAssignmentValueSymbol(node.parent)
      : checker.getSymbolAtLocation(node);
  const local = (node) =>
    Boolean(
      symbolAt(node)?.declarations?.some(
        (declaration) =>
          ts.isVariableDeclaration(declaration) ||
          ts.isParameter(declaration) ||
          ts.isBindingElement(declaration) ||
          ts.isFunctionDeclaration(declaration) ||
          ts.isFunctionExpression(declaration) ||
          ts.isClassDeclaration(declaration) ||
          ts.isClassExpression(declaration) ||
          ts.isImportSpecifier(declaration) ||
          ts.isImportClause(declaration) ||
          ts.isNamespaceImport(declaration),
      ),
    );
  const rootOf = (expression, seen = new Set()) => {
    const node = unwrap(expression);
    if (ts.isIdentifier(node)) {
      if (
        (["globalThis", "window", "self"].includes(node.text) ||
          GLOBAL_NAMESPACES.has(node.text)) &&
        !local(node)
      )
        return node.text;
      const symbol = symbolAt(node);
      if (symbol && !seen.has(symbol)) {
        seen.add(symbol);
        const declaration = symbol.valueDeclaration;
        if (
          declaration &&
          ts.isVariableDeclaration(declaration) &&
          declaration.initializer
        )
          return rootOf(declaration.initializer, seen);
        if (
          declaration &&
          ts.isBindingElement(declaration) &&
          ts.isObjectBindingPattern(declaration.parent)
        ) {
          const variable = declaration.parent.parent;
          const name = bindingName(declaration);
          if (
            ts.isVariableDeclaration(variable) &&
            variable.initializer &&
            GLOBAL_NAMESPACES.has(name) &&
            ["globalThis", "window", "self"].includes(
              rootOf(variable.initializer, seen),
            )
          )
            return name;
        }
      }
    }
    if (
      ts.isPropertyAccessExpression(node) ||
      ts.isElementAccessExpression(node)
    ) {
      const name = memberName(node);
      if (
        GLOBAL_NAMESPACES.has(name) &&
        ["globalThis", "window", "self"].includes(rootOf(node.expression, seen))
      )
        return name;
    }
    return null;
  };
  // Function type checks and prototype methods do not compile source text.
  // Keep every alias confined to static method reads or calls. The prototype
  // constructor, computed members, mutations, and outgoing references stay forbidden.
  let references;
  const functionPrototypeMethods = new Set([
    "toString",
    "bind",
    "call",
    "apply",
  ]);
  const pureAliasUses = (declaration, seen, check) => {
    const statement = declaration.parent?.parent;
    if (
      statement?.modifiers?.some(
        (modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword,
      )
    )
      return false;
    const symbol = symbolAt(declaration.name);
    if (!symbol || seen.has(symbol)) return false;
    if (!references) {
      references = new Map();
      const collect = (child) => {
        if (ts.isIdentifier(child)) {
          const declaration = child.parent;
          const target = ts.isExportSpecifier(declaration)
            ? checker.getExportSpecifierLocalTargetSymbol(declaration)
            : symbolAt(child);
          if (target) {
            const uses = references.get(target) ?? [];
            uses.push(child);
            references.set(target, uses);
          }
        }
        ts.forEachChild(child, collect);
      };
      collect(source);
    }
    const aliases = new Set(seen).add(symbol);
    return (references.get(symbol) ?? []).every(
      (reference) =>
        reference === declaration.name || check(reference, aliases),
    );
  };
  const pureFunctionResult = (node, seen = new Set()) => {
    let value = node;
    while (value.parent && unwrap(value.parent) === value) value = value.parent;
    const parent = value.parent;
    if (
      (ts.isPropertyAccessExpression(parent) ||
        ts.isElementAccessExpression(parent)) &&
      parent.expression === value
    ) {
      const name = memberName(parent);
      return (
        name !== null &&
        name !== "constructor" &&
        pureFunctionResult(parent, seen)
      );
    }
    if (ts.isCallExpression(parent) && parent.expression === value)
      return pureFunctionResult(parent, seen);
    if (ts.isVariableDeclaration(parent) && parent.initializer === value) {
      return (
        ts.isIdentifier(parent.name) &&
        pureAliasUses(parent, seen, pureFunctionResult)
      );
    }
    return true;
  };
  const pureFunctionValue = (node, kind = "constructor", seen = new Set()) => {
    let value = node;
    while (value.parent && unwrap(value.parent) === value) value = value.parent;
    const parent = value.parent;
    if (
      ts.isBinaryExpression(parent) &&
      parent.operatorToken.kind === ts.SyntaxKind.InstanceOfKeyword &&
      parent.right === value
    )
      return true;
    if (
      (ts.isPropertyAccessExpression(parent) ||
        ts.isElementAccessExpression(parent)) &&
      parent.expression === value
    ) {
      const name = memberName(parent);
      if (kind === "constructor")
        return (
          name === "prototype" && pureFunctionValue(parent, "prototype", seen)
        );
      return (
        functionPrototypeMethods.has(name) &&
        pureFunctionValue(parent, "method", seen)
      );
    }
    if (kind === "constructor") return false;
    if (ts.isCallExpression(parent) && parent.expression === value)
      return kind === "method" && pureFunctionResult(parent);
    if (
      ts.isVariableDeclaration(parent) &&
      parent.initializer === value &&
      ts.isIdentifier(parent.name)
    ) {
      return pureAliasUses(parent, seen, (reference, aliases) =>
        pureFunctionValue(reference, kind, aliases),
      );
    }
    return false;
  };
  const add = (node, capability, detail) => {
    if (capability === "business-timer" && permissions.ui) return;
    if (capability === "browser-persistence" && permissions.readonly) {
      let access = node;
      while (access.parent && unwrap(access.parent) === access)
        access = access.parent;
      const member = access.parent;
      const name =
        member &&
        (ts.isPropertyAccessExpression(member) ||
          ts.isElementAccessExpression(member)) &&
        member.expression === access
          ? memberName(member)
          : null;
      const read =
        detail === "indexedDB"
          ? name === "databases"
          : READONLY_STORAGE_MEMBERS.has(name);
      let target = member;
      while (target?.parent && unwrap(target.parent) === target)
        target = target.parent;
      const parent = target?.parent;
      const write =
        parent &&
        ((ts.isBinaryExpression(parent) &&
          parent.left === target &&
          parent.operatorToken.kind >= ts.SyntaxKind.FirstAssignment &&
          parent.operatorToken.kind <= ts.SyntaxKind.LastAssignment) ||
          ts.isDeleteExpression(parent) ||
          ((ts.isPrefixUnaryExpression(parent) ||
            ts.isPostfixUnaryExpression(parent)) &&
            [
              ts.SyntaxKind.PlusPlusToken,
              ts.SyntaxKind.MinusMinusToken,
            ].includes(parent.operator)));
      if (
        read &&
        !write &&
        (name === "length" ||
          (ts.isCallExpression(member.parent) &&
            member.parent.expression === member))
      )
        return;
    }
    const { line, character } = source.getLineAndCharacterOfPosition(
      node.getStart(source),
    );
    findings.push({
      capability,
      detail,
      line: line + 1,
      column: character + 1,
    });
  };
  const visit = (node) => {
    if (
      ts.isTypeNode(node) ||
      ts.isInterfaceDeclaration(node) ||
      ts.isImportDeclaration(node) ||
      ts.isExportDeclaration(node)
    )
      return;
    if (
      ts.isCallExpression(node) &&
      node.expression.kind === ts.SyntaxKind.ImportKeyword &&
      !ts.isStringLiteralLike(node.arguments[0])
    )
      add(node, "unresolved-dynamic-import", "import target is not a literal");
    if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
      for (const argument of node.arguments ?? []) {
        if (rootOf(argument))
          add(
            argument,
            "browser-global-escape",
            "browser global passed to another owner",
          );
      }
    }
    if (
      (ts.isReturnStatement(node) &&
        node.expression &&
        rootOf(node.expression)) ||
      (ts.isPropertyAssignment(node) && rootOf(node.initializer)) ||
      (ts.isArrayLiteralExpression(node) &&
        node.elements.some((element) => rootOf(element)))
    )
      add(
        node,
        "browser-global-escape",
        "browser global returned or stored in another owner",
      );
    if (
      ts.isIdentifier(node) &&
      GLOBAL_CAPABILITIES.has(node.text) &&
      !local(node) &&
      !(node.text === "Function" && pureFunctionValue(node))
    ) {
      const parent = node.parent;
      if (
        !(ts.isPropertyAccessExpression(parent) && parent.name === node) &&
        !(ts.isPropertyAssignment(parent) && parent.name === node) &&
        !(ts.isJsxAttribute(parent) && parent.name === node) &&
        !ts.isJsxNamespacedName(parent) &&
        !(
          (ts.isJsxOpeningElement(parent) ||
            ts.isJsxSelfClosingElement(parent) ||
            ts.isJsxClosingElement(parent)) &&
          parent.tagName === node &&
          /^[a-z]/.test(node.text)
        ) &&
        !(ts.isBindingElement(parent) && parent.propertyName === node) &&
        !(
          (ts.isMethodDeclaration(parent) ||
            ts.isPropertyDeclaration(parent) ||
            ts.isGetAccessor(parent) ||
            ts.isSetAccessor(parent)) &&
          parent.name === node
        )
      )
        add(node, GLOBAL_CAPABILITIES.get(node.text), node.text);
    }
    if (
      ts.isPropertyAccessExpression(node) ||
      ts.isElementAccessExpression(node)
    ) {
      const owner = rootOf(node.expression);
      const name = memberName(node);
      if (owner && !name)
        add(node, "dynamic-browser-global", "computed global member");
      if (
        ["globalThis", "window", "self"].includes(owner) &&
        GLOBAL_CAPABILITIES.has(name)
      )
        add(node, GLOBAL_CAPABILITIES.get(name), name);
      if (owner === "navigator" && NAVIGATOR_CAPABILITIES.has(name))
        add(node, NAVIGATOR_CAPABILITIES.get(name), `navigator.${name}`);
      if (owner === "URL" && OBJECT_URL_MEMBERS.has(name))
        add(node, "browser-bytes", `URL.${name}`);
      if (owner === "document" && name === "cookie")
        add(node, "browser-persistence", "document.cookie");
    }
    if (
      ts.isVariableDeclaration(node) &&
      ts.isObjectBindingPattern(node.name) &&
      node.initializer &&
      rootOf(node.initializer)
    ) {
      const owner = rootOf(node.initializer);
      for (const element of node.name.elements) {
        const name = bindingName(element);
        if (!name)
          add(element, "dynamic-browser-global", "computed global member");
        if (
          ["globalThis", "window", "self"].includes(owner) &&
          GLOBAL_CAPABILITIES.has(name)
        )
          add(element, GLOBAL_CAPABILITIES.get(name), name);
        if (owner === "navigator" && NAVIGATOR_CAPABILITIES.has(name))
          add(element, NAVIGATOR_CAPABILITIES.get(name), `navigator.${name}`);
        if (owner === "URL" && OBJECT_URL_MEMBERS.has(name))
          add(element, "browser-bytes", `URL.${name}`);
        if (owner === "document" && name === "cookie")
          add(element, "browser-persistence", "document.cookie");
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return findings;
}

// This owner binds a terminal build audit to the exact emitted file bytes.
export const NATIVE_AUDIT_FILENAME = "native-boundary-audit.json";
const AUDIT_VERSION = 1;
const sessions = new WeakMap();
const digest = (value) => createHash("sha256").update(value).digest("hex");
const vendor = (id) => cleanId(id).includes("/node_modules/");
const bytes = (item) =>
  item.type === "chunk" ? Buffer.from(item.code) : Buffer.from(item.source);
const labelFor = (root, id) =>
  isAbsolute(cleanId(id)) ? relative(root, cleanId(id)) : id;
function reject(findings) {
  if (findings.length)
    throw new Error(
      "Native import graph rejected:\n" +
        findings
          .map(
            (f) =>
              `${(f.trace ?? [f.module]).join(" -> ")}:${f.line ?? 0}:${f.column ?? 0} [${f.capability}] ${f.detail}`,
          )
          .join("\n"),
    );
}
function safeName(name) {
  if (
    typeof name !== "string" ||
    !name ||
    name.includes("\\") ||
    isAbsolute(name) ||
    name.split("/").some((part) => !part || part === "." || part === "..")
  )
    throw new Error(`Unknown native emission path: ${name}`);
  return name;
}
function graphContract(root, options) {
  const entries = (items) =>
    items
      .map(({ path, reason }) => ({
        path: canonical(resolvePath(root, path)),
        reason,
      }))
      .sort((a, b) => a.path.localeCompare(b.path));
  return {
    entry: canonical(resolvePath(root, options.entry)),
    uiAdapters: entries(options.uiAdapters ?? []),
    readonlyMigrations: entries(options.readonlyMigrations ?? []),
  };
}
function sourceKind(id) {
  const path = cleanId(id);
  if (path.endsWith(".tsx")) return ts.ScriptKind.TSX;
  if (path.endsWith(".ts")) return ts.ScriptKind.TS;
  if (path.endsWith(".jsx")) return ts.ScriptKind.JSX;
  if (path.endsWith(".json")) return ts.ScriptKind.JSON;
  return ts.ScriptKind.JS;
}
function inspectOriginalCapabilities(source, permissions) {
  // Raw code is strict before transforms. Non-code UI assets use the terminal asset owner.
  if (
    /\.(?:css|html|svg|png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf)$/i.test(
      source.id,
    )
  )
    return [];
  if (isAbsolute(source.id) && !/\.(?:[cm]?[jt]sx?|json)$/i.test(source.id))
    return [
      {
        capability: "uninspected-source",
        detail: "The native source grammar is unknown.",
      },
    ];
  return inspectSourceCapabilities(
    source.raw,
    source.id,
    permissions,
    sourceKind(source.id),
  );
}

/** Project sources stay strict, including erased and guarded dynamic imports. */
export function nativeImportGraphPlugin({
  entry,
  uiAdapters = [],
  readonlyMigrations = [],
}) {
  const state = { sources: new Map(), outputs: [], report: undefined };
  const plugin = {
    name: "studio-native-import-graph",
    enforce: "pre",
    configResolved(config) {
      state.root = canonical(config.root);
      state.contract = graphContract(state.root, {
        entry,
        uiAdapters,
        readonlyMigrations,
      });
      state.ui = exceptionPaths(state.root, uiAdapters, "UI");
      state.readonly = exceptionPaths(
        state.root,
        readonlyMigrations,
        "Read-only migration",
      );
      state.write = config.build.write !== false;
    },
    async buildStart() {
      state.sources.clear();
      state.outputs = [];
      state.report = undefined;
      const resolved = await this.resolve(resolvePath(state.root, entry));
      if (!resolved || resolved.external)
        this.error(`Native entry cannot be resolved: ${entry}`);
      state.entry = resolved.id;
    },
    moduleParsed(info) {
      if (typeof info.code !== "string") return;
      const id = canonical(info.id);
      const rawBytes =
        isAbsolute(cleanId(info.id)) && existsSync(cleanId(info.id))
          ? readFileSync(cleanId(info.id))
          : Buffer.from(info.code);
      const raw = rawBytes.toString("utf8");
      state.sources.set(info.id, {
        id,
        code: info.code,
        raw,
        sourceHash: digest(rawBytes),
        transformedHash: digest(info.code),
      });
    },
    buildEnd(error) {
      if (error) return;
      const modules = new Map(
        [...this.getModuleIds()].map((id) => [id, this.getModuleInfo(id)]),
      );
      if (!modules.has(state.entry))
        this.error(`Native entry is absent from the build graph: ${entry}`);
      state.parents = new Map([[state.entry, null]]);
      const queue = [state.entry];
      const findings = [];
      for (const id of queue) {
        const info = modules.get(id);
        if (!info)
          this.error(`Native dependency has no resolved graph metadata: ${id}`);
        const owner = canonical(id);
        const packagePath = id.split("/node_modules/").at(-1);
        const packageName = packagePath?.startsWith("@")
          ? packagePath.split("/").slice(0, 2).join("/")
          : packagePath?.split("/")[0];
        if (
          (vendor(id) && BROWSER_PACKAGES.has(packageName)) ||
          /(?:^|:)node:(?:crypto|http|https|net|tls)(?:$|\?)/.test(id)
        )
          findings.push({
            module: id,
            capability: "browser-owner",
            detail: packageName || id,
          });
        if (info.isExternal)
          findings.push({
            module: id,
            capability: "uninspected-external",
            detail: id,
          });
        const source = state.sources.get(id);
        if (source && !vendor(id)) {
          const permissions = {
            ui: state.ui.has(owner),
            readonly: state.readonly.has(owner),
          };
          findings.push(
            ...inspectOriginalCapabilities(source, permissions).map((f) => ({
              module: id,
              ...f,
            })),
            ...inspectCapabilities(source.code, owner, permissions).map(
              (f) => ({ module: id, ...f }),
            ),
          );
        } else if (!source && !info.isExternal)
          findings.push({
            module: id,
            capability: "uninspected-module",
            detail: "Resolved module has no inspectable JavaScript.",
          });
        for (const child of [
          ...info.importedIds,
          ...info.dynamicallyImportedIds,
        ])
          if (!state.parents.has(child)) {
            state.parents.set(child, id);
            queue.push(child);
          }
      }
      state.queue = queue;
      for (const f of findings) f.trace = trace(state, f.module);
      state.report = {
        entry: state.entry,
        modules: queue.map((id) => labelFor(state.root, id)),
        findings,
        accepted: false,
      };
      reject(findings);
    },
    generateBundle: {
      order: "post",
      handler(options, bundle) {
        state.outputs.push({
          bundle,
          dir: options.dir
            ? resolvePath(state.root, options.dir)
            : options.file
              ? dirname(resolvePath(state.root, options.file))
              : undefined,
        });
      },
    },
  };
  const checker = {
    plugin,
    report: () => state.report,
    finalize: (output) => finalizeNativeImportGraph(checker, output),
  };
  sessions.set(checker, state);
  return checker;
}
function trace(state, module) {
  const result = [];
  for (let id = module; id != null; id = state.parents.get(id))
    result.unshift(labelFor(state.root, id));
  return result;
}

function sourceMap(state, item, outputDir) {
  if (!item.map) {
    if (item.code.trim())
      throw new Error(
        `Unknown native emission without a source map: ${item.fileName}`,
      );
    return { segments: [], mapped: new Set() };
  }
  const map = typeof item.map === "string" ? JSON.parse(item.map) : item.map;
  if (
    map.version !== 3 ||
    !Array.isArray(map.sources) ||
    !Array.isArray(map.sourcesContent) ||
    map.sources.length !== map.sourcesContent.length ||
    typeof map.mappings !== "string" ||
    (map.sourceRoot && map.sourceRoot !== "")
  )
    throw new Error(`Unknown native emission source map: ${item.fileName}`);
  const sourceIds = map.sources.map((name, index) => {
    if (
      typeof name !== "string" ||
      typeof map.sourcesContent[index] !== "string"
    )
      throw new Error("Unknown native source map content");
    const resolved = canonical(
      resolvePath(outputDir, dirname(item.fileName), name),
    );
    const matches = [...state.sources.values()].filter(
      (source) => source.id === resolved || source.id === name,
    );
    if (
      matches.length !== 1 ||
      ![matches[0].raw, matches[0].code].includes(map.sourcesContent[index])
    )
      throw new Error(`Unknown native source map owner: ${name}`);
    return matches[0];
  });
  const decoder = ts.decodeMappings(map.mappings);
  const segments = [];
  const mapped = new Set();
  for (let step = decoder.next(); !step.done; step = decoder.next()) {
    const segment = { ...step.value };
    if (segment.sourceIndex !== undefined) {
      const source = sourceIds[segment.sourceIndex];
      if (
        !source ||
        segment.sourceLine < 0 ||
        segment.sourceCharacter < 0 ||
        segment.sourceLine >=
          map.sourcesContent[segment.sourceIndex].split("\n").length
      )
        throw new Error("Unknown native source map position");
      segment.source = source;
      mapped.add(source.id);
    }
    segments.push(segment);
  }
  if (decoder.error)
    throw new Error(`Invalid native source map: ${decoder.error}`);
  return { segments, mapped };
}
function permissionAt(state, finding, mappings) {
  const line = finding.line - 1;
  const column = finding.column - 1;
  // Exact generated token mappings prevent late appended code from inheriting an earlier owner.
  const candidates = mappings.segments.filter(
    (s) =>
      s.generatedLine === line && s.generatedCharacter === column && s.source,
  );
  return candidates.some((segment) => {
    const owner = segment.source.id;
    const allowed =
      finding.capability === "business-timer"
        ? state.ui.has(owner)
        : state.readonly.has(owner);
    if (!allowed) return false;
    const original = inspectSourceCapabilities(
      segment.source.raw,
      owner,
      { ui: false, readonly: false },
      sourceKind(owner),
    );
    const point = original.find(
      (f) =>
        f.line === segment.sourceLine + 1 &&
        f.column === segment.sourceCharacter + 1 &&
        f.capability === finding.capability &&
        f.detail === finding.detail,
    );
    if (!point) return false;
    const permitted = inspectSourceCapabilities(
      segment.source.raw,
      owner,
      { ui: state.ui.has(owner), readonly: state.readonly.has(owner) },
      sourceKind(owner),
    );
    return !permitted.some(
      (f) =>
        f.line === point.line &&
        f.column === point.column &&
        f.capability === point.capability &&
        f.detail === point.detail,
    );
  });
}
async function diskFiles(dir, prefix = "") {
  const result = [];
  for (const entry of await readdir(resolvePath(dir, prefix), {
    withFileTypes: true,
  })) {
    const name = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isSymbolicLink())
      throw new Error(`Native artifact symlink is not allowed: ${name}`);
    if (entry.isDirectory()) result.push(...(await diskFiles(dir, name)));
    else if (entry.isFile()) result.push(name);
    else throw new Error(`Unknown native artifact file: ${name}`);
  }
  return result.sort();
}
async function compareDisk(dir, artifacts, allowReceipt) {
  if (!(await lstat(dir)).isDirectory() || (await lstat(dir)).isSymbolicLink())
    throw new Error("Native artifact directory is not a real directory");
  const names = (await diskFiles(dir)).filter(
    (name) => !(allowReceipt && name === NATIVE_AUDIT_FILENAME),
  );
  if (
    JSON.stringify(names) !==
    JSON.stringify(artifacts.map((a) => a.path).sort())
  )
    throw new Error(
      "Native artifact emission inventory differs from written files",
    );
  for (const artifact of artifacts) {
    const data = await readFile(resolvePath(dir, artifact.path));
    if (data.length !== artifact.bytes || digest(data) !== artifact.sha256)
      throw new Error(`Native artifact hash mismatch: ${artifact.path}`);
  }
}

/** Call after await build(). No plugin hook can accept a final native artifact. */
export async function finalizeNativeImportGraph(checker, result) {
  const state = sessions.get(checker);
  if (!state?.report || state.finalized)
    throw new Error("Native emission audit has no unfinished build session");
  const results = Array.isArray(result) ? result : [result];
  if (results.length !== state.outputs.length || !results.length)
    throw new Error("Native emission result does not match build outputs");
  const inventory = state.queue.map((id) => {
    const source = state.sources.get(id);
    return {
      id: source.id,
      sourceHash: source.sourceHash,
      transformedHash: source.transformedHash,
      disposition: "omitted",
      renderedBytes: 0,
      emissions: [],
    };
  });
  const byId = new Map(inventory.map((source) => [source.id, source]));
  const findings = [];
  const artifacts = [];
  let outDir;
  for (let index = 0; index < results.length; index++) {
    const captured = state.outputs[index];
    if (!Array.isArray(results[index]?.output) || !captured.dir)
      throw new Error("Unknown native emission output");
    if (outDir && outDir !== captured.dir)
      throw new Error("Native emission audit requires one output directory");
    outDir = captured.dir;
    const returned = new Map(
      results[index].output.map((item) => [safeName(item.fileName), item]),
    );
    const capturedItems = Object.values(captured.bundle);
    if (
      returned.size !== results[index].output.length ||
      capturedItems.some((item) => !returned.has(item.fileName))
    )
      throw new Error(
        "Native emission result inventory differs from build output",
      );
    for (const item of results[index].output) {
      safeName(item.fileName);
      const data = bytes(item);
      artifacts.push({
        path: item.fileName,
        bytes: data.length,
        sha256: digest(data),
      });
      if (item.type === "chunk") {
        const mappings = sourceMap(state, item, outDir);
        const seen = new Set();
        for (const [id, metadata] of Object.entries(item.modules)) {
          const owner = canonical(id);
          const row = byId.get(owner);
          const source = state.sources.get(id);
          if (
            !row ||
            !source ||
            !Number.isFinite(metadata.renderedLength) ||
            metadata.renderedLength < 0 ||
            !(metadata.code === null || typeof metadata.code === "string")
          )
            throw new Error(`Unknown native emission module: ${id}`);
          seen.add(owner);
          const rendered = metadata.code ?? "";
          const retained =
            metadata.renderedLength > 0 ||
            rendered.length > 0 ||
            mappings.mapped.has(owner);
          if (retained) {
            row.disposition = "retained";
            row.renderedBytes += Buffer.byteLength(rendered);
            row.emissions.push({
              artifact: item.fileName,
              renderedHash: digest(rendered),
              renderedBytes: Buffer.byteLength(rendered),
            });
            if (vendor(id)) {
              findings.push(
                ...inspectOriginalCapabilities(source, {
                  ui: state.ui.has(owner),
                  readonly: state.readonly.has(owner),
                }).map((f) => ({ module: id, ...f, trace: trace(state, id) })),
              );
              findings.push(
                ...inspectCapabilities(source.code, owner, {
                  ui: state.ui.has(owner),
                  readonly: state.readonly.has(owner),
                }).map((f) => ({ module: id, ...f, trace: trace(state, id) })),
              );
            }
          }
        }
        for (const owner of mappings.mapped)
          if (!seen.has(owner))
            throw new Error(`Native emission mapping has no module: ${owner}`);
        for (const finding of inspectCapabilities(item.code, item.fileName, {
          ui: false,
          readonly: false,
        })) {
          if (!permissionAt(state, finding, mappings))
            findings.push({
              module: item.fileName,
              ...finding,
              trace: [item.fileName],
            });
        }
        inspectEmittedImports(item.code, item.fileName, returned);
        for (const imported of [...item.imports, ...item.dynamicImports])
          if (!returned.has(imported))
            throw new Error(`Uninspected native emitted import: ${imported}`);
      } else inspectAsset(item, findings, returned);
    }
  }
  state.report.inventory = {
    modules: inventory,
    artifacts: artifacts.sort((a, b) => a.path.localeCompare(b.path)),
  };
  state.report.findings.push(...findings);
  reject(findings);
  await verifySources(inventory);
  if (state.write) await compareDisk(outDir, artifacts, true);
  const receipt = {
    version: AUDIT_VERSION,
    auditorHash: digest(readFileSync(new URL(import.meta.url))),
    entry: state.entry,
    contract: state.contract,
    sourceInventoryHash: digest(JSON.stringify(inventory)),
    sources: inventory,
    artifacts: state.report.inventory.artifacts,
  };
  receipt.hash = digest(JSON.stringify(receipt));
  if (state.write)
    await writeFile(
      resolvePath(outDir, NATIVE_AUDIT_FILENAME),
      JSON.stringify(receipt, null, 2) + "\n",
    );
  state.report.accepted = true;
  state.report.receipt = receipt;
  state.finalized = true;
  return state.report;
}
function emittedTarget(specifier, owner, returned, executable = false) {
  if (
    typeof specifier !== "string" ||
    !specifier ||
    /[?#\\]/.test(specifier) ||
    /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i.test(specifier)
  )
    throw new Error(`Uninspected native emitted import: ${specifier}`);
  const name = relative("/", resolvePath("/", dirname(owner), specifier));
  const target = returned.get(name);
  if (
    !target ||
    (executable && target.type !== "chunk" && !/\.[cm]?js$/i.test(name))
  )
    throw new Error(`Uninspected native emitted import: ${specifier}`);
  return name;
}
function inspectEmittedImports(code, owner, returned) {
  const source = ts.createSourceFile(
    "emitted.js",
    code,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.JS,
  );
  const visit = (node) => {
    if (
      (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) &&
      node.moduleSpecifier
    )
      emittedTarget(
        ts.isStringLiteralLike(node.moduleSpecifier)
          ? node.moduleSpecifier.text
          : undefined,
        owner,
        returned,
        true,
      );
    if (
      ts.isCallExpression(node) &&
      node.expression.kind === ts.SyntaxKind.ImportKeyword
    )
      emittedTarget(
        node.arguments.length === 1 && ts.isStringLiteralLike(node.arguments[0])
          ? node.arguments[0].text
          : undefined,
        owner,
        returned,
        true,
      );
    ts.forEachChild(node, visit);
  };
  visit(source);
}
function inspectCss(content, name, returned) {
  const decoded = content
    .replace(/\\([0-9a-f]{1,6})\s?|\\([^\r\n])/gi, (_match, hex, literal) =>
      hex ? String.fromCodePoint(parseInt(hex, 16)) : literal,
    )
    .replace(/\/\*[\s\S]*?\*\//g, "");
  // String-based image and font sources have no target parser in this audit.
  if (
    /@import|expression\s*\(|(?:^|[^a-z0-9_-])(?:-webkit-)?(?:image-set|image|src)\s*\(/i.test(
      decoded,
    )
  )
    throw new Error(`Uninspected native CSS capability: ${name}`);
  const starts = [...decoded.matchAll(/url\s*\(/gi)];
  const urls = [
    ...decoded.matchAll(
      /url\s*\(\s*(?:"([^"]*)"|'([^']*)'|([^\s)'";]+))\s*\)/gi,
    ),
  ];
  if (starts.length !== urls.length)
    throw new Error(`Unknown native CSS URL: ${name}`);
  for (const match of urls) {
    const target = match[1] ?? match[2] ?? match[3];
    if (/^data:image\/(?:png|jpeg|gif|webp);base64,[a-z0-9+/=]+$/i.test(target))
      continue;
    if (target.startsWith("#")) continue;
    emittedTarget(target, name, returned);
  }
}
function inspectAsset(item, findings, returned) {
  const name = item.fileName;
  const content = bytes(item).toString("utf8");
  if (/\.(?:[cm]?js)$/i.test(name)) {
    findings.push(
      ...inspectCapabilities(content, name, { ui: false, readonly: false }).map(
        (f) => ({ module: name, ...f, trace: [name] }),
      ),
    );
    inspectEmittedImports(content, name, returned);
  } else if (/\.html$/i.test(name)) {
    const scripts = [
      ...content.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script\s*>/gi),
    ];
    if (scripts.length !== [...content.matchAll(/<script\b/gi)].length)
      throw new Error(`Unknown native HTML script: ${name}`);
    for (const [, attributes, inline] of scripts) {
      const src = attributes.match(/\bsrc\s*=\s*["']([^"']+)["']/i)?.[1];
      if (/\bsrc\s*=/i.test(attributes) && !src)
        throw new Error(`Unknown native HTML script source: ${name}`);
      if (src) emittedTarget(src, name, returned, true);
      if (inline.trim()) {
        findings.push(
          ...inspectCapabilities(inline, name, {
            ui: false,
            readonly: false,
          }).map((f) => ({ module: name, ...f, trace: [name] })),
        );
        inspectEmittedImports(inline, name, returned);
      }
    }
    // The native document uses explicit src/href assets. Other resource forms hold.
    if (
      /\b(?:srcset|imagesrcset|style|poster|action|formaction|data|codebase|archive|background|longdesc|profile|manifest|ping|cite|lowsrc|dynsrc|srcdoc|http-equiv)\s*=/i.test(
        content,
      )
    )
      throw new Error(`Uninspected native HTML resource attribute: ${name}`);
    if (
      /\bon[a-z]+\s*=|javascript\s*:|<base\b|<iframe\b|<object\b|<embed\b|<applet\b/i.test(
        content,
      )
    )
      throw new Error(`Uninspected native HTML executable attribute: ${name}`);
    for (const match of content.matchAll(
      /\b(?:src|href)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/gi,
    )) {
      const target = match[1] ?? match[2] ?? match[3];
      if (!target.startsWith("#")) emittedTarget(target, name, returned);
    }
    for (const match of content.matchAll(
      /<style\b[^>]*>([\s\S]*?)<\/style\s*>/gi,
    ))
      inspectCss(match[1], name, returned);
    for (const match of content.matchAll(/<svg\b[\s\S]*?<\/svg\s*>/gi))
      inspectSvg(match[0], name);
  } else if (/\.map$/i.test(name)) {
    const parent = returned.get(name.slice(0, -4));
    if (
      !parent ||
      parent.type !== "chunk" ||
      !parent.map ||
      digest(content.trim()) !==
        digest(
          typeof parent.map === "string" ? parent.map : parent.map.toString(),
        )
    )
      throw new Error(`Unknown native emission map asset: ${name}`);
  } else if (/\.css$/i.test(name)) inspectCss(content, name, returned);
  else if (/\.svg$/i.test(name)) inspectSvg(content, name);
  else if (!/\.(?:png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf)$/i.test(name))
    throw new Error(`Unknown native asset format: ${name}`);
}

function inspectSvg(content, name) {
  if (
    /<(?:script|style|foreignObject|iframe|object|embed|animate\w*|set)\b|\bon[a-z]+\s*=|\b(?:href|src|style)\s*=|url\s*\(|<!ENTITY|<!DOCTYPE/i.test(
      content,
    )
  )
    throw new Error(`Uninspected native SVG capability: ${name}`);
}

async function verifySources(sources) {
  for (const source of sources) {
    if (
      typeof source.id !== "string" ||
      !/^[a-f0-9]{64}$/.test(source.sourceHash)
    )
      throw new Error("Unknown native source inventory");
    if (
      isAbsolute(source.id) &&
      digest(await readFile(source.id)) !== source.sourceHash
    )
      throw new Error(`Native artifact source is stale: ${source.id}`);
  }
}
export async function verifyNativeArtifactAudit({ outDir, expected }) {
  const receipt = JSON.parse(
    await readFile(resolvePath(outDir, NATIVE_AUDIT_FILENAME), "utf8"),
  );
  const { hash, ...body } = receipt;
  if (
    receipt.version !== AUDIT_VERSION ||
    hash !== digest(JSON.stringify(body)) ||
    receipt.auditorHash !== digest(readFileSync(new URL(import.meta.url))) ||
    !Array.isArray(receipt.artifacts) ||
    !Array.isArray(receipt.sources) ||
    receipt.sourceInventoryHash !== digest(JSON.stringify(receipt.sources))
  )
    throw new Error("Missing or stale native artifact audit");
  if (
    expected &&
    JSON.stringify(receipt.contract) !==
      JSON.stringify(graphContract(canonical(expected.root), expected))
  )
    throw new Error(
      "Native artifact audit has another entry or permission contract",
    );
  for (const artifact of receipt.artifacts) safeName(artifact.path);
  await verifySources(receipt.sources);
  await compareDisk(outDir, receipt.artifacts, true);
  return receipt;
}
export async function buildNativeRenderer(config, graphOptions) {
  const checker = nativeImportGraphPlugin(graphOptions);
  const output = await build({
    ...config,
    plugins: [...(config.plugins ?? []), checker.plugin],
    build: { ...config.build, sourcemap: "hidden" },
  });
  const report = await finalizeNativeImportGraph(checker, output);
  return { output, report, receipt: report.receipt };
}
export async function checkNativeImportGraph({
  root,
  entry,
  resolve = {},
  uiAdapters = [],
  readonlyMigrations = [],
}) {
  const { report } = await buildNativeRenderer(
    {
      root,
      configFile: false,
      logLevel: "silent",
      resolve,
      build: { write: false, minify: false, rolldownOptions: { input: entry } },
    },
    { entry, uiAdapters, readonlyMigrations },
  );
  return report;
}
