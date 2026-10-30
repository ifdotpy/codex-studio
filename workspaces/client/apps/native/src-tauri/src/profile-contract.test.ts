import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import ts from "typescript";

// importBrowserProfile.ts owns the full wire contract. Rust intentionally
// reads only these fields and keeps unselected snapshots and records opaque.
test("native archive views track the browser profile contract and callers", () => {
  const read = (path: string) => readFileSync(path, "utf8");
  const ownerPath =
    "workspaces/client/apps/web/src/native/importBrowserProfile.ts";
  const owner = ts.createSourceFile(
    ownerPath,
    read(ownerPath),
    ts.ScriptTarget.Latest,
    true,
  );
  const rust = read("workspaces/client/apps/native/src-tauri/src/profile.rs");
  const views = [
    [
      "BrowserProfileArchive",
      "Archive",
      {
        id: ["string", "String", false],
        version: ["1 | 2", "u64", false],
        owners: [
          "Record<string, string>",
          "BTreeMap<String, Field<String>>",
          false,
        ],
        snapshots: ["BrowserProfileSnapshot[]", "Vec<Value>", false],
        parts: ["string", "String", true],
      },
    ],
    [
      "BrowserProfileSnapshot",
      "Origin",
      { origin: ["string", "&'a str", true] },
    ],
    [
      "BrowserProfileSnapshot",
      "ChunkSnapshot",
      {
        databases: [
          "BrowserProfileDatabase[]",
          "Vec<Field<Object<ChunkDatabase>>>",
          false,
        ],
      },
    ],
    [
      "BrowserProfileDatabase",
      "ChunkDatabase",
      {
        stores: [
          "BrowserProfileStore[]",
          "Vec<Field<Object<ChunkStore>>>",
          false,
        ],
      },
    ],
    [
      "BrowserProfileStore",
      "ChunkStore",
      {
        chunks: [
          "BrowserProfileChunk[]",
          "Vec<Field<Object<ChunkReference>>>",
          true,
        ],
      },
    ],
    [
      "BrowserProfileChunk",
      "ChunkReference",
      {
        sha256: ["string", "String", false],
        bytes: ["number", "u64", false],
        records: ["number", "u64", false],
      },
    ],
  ] as const;
  for (const [name, view, fields] of views) {
    const declaration = owner.statements.find(
      (s) => ts.isTypeAliasDeclaration(s) && s.name.text === name,
    );
    assert.ok(
      declaration &&
        ts.isTypeAliasDeclaration(declaration) &&
        ts.isTypeLiteralNode(declaration.type),
      name,
    );
    const native = rust.match(
      new RegExp(`struct ${view}(?:<[^>]+>)? \\{([^}]+)\\}`),
    )?.[1];
    assert.ok(native, view);
    for (const [field, [type, rustType, optional]] of Object.entries(fields)) {
      const member = declaration.type.members.find(
        (m) => ts.isPropertySignature(m) && m.name.getText(owner) === field,
      );
      assert.ok(
        member && ts.isPropertySignature(member) && member.type,
        `${name}.${field}`,
      );
      assert.equal(member.type.getText(owner), type, `${name}.${field}`);
      assert.equal(
        Boolean(member.questionToken),
        optional,
        `${name}.${field} optionality`,
      );
      assert.ok(
        native.includes(`${field}: Field<${rustType}>`),
        `${view}.${field}`,
      );
    }
  }
  // The importer owns the accepted chunk bound; the native read guard must
  // enforce the same byte limit before opening and hashing the file.
  assert.match(rust, /const MAX_CHUNK_BYTES: u64 = 64 \* 1024 \* 1024;/);
  assert.match(rust, /metadata\.len\(\) > MAX_CHUNK_BYTES/);
  assert.match(rust, /\.take\(MAX_CHUNK_BYTES \+ 1\)/);
  const limits: string[] = [];
  const findLimits = (node: ts.Node) => {
    if (
      ts.isBinaryExpression(node) &&
      node.operatorToken.kind === ts.SyntaxKind.GreaterThanToken &&
      node.left.getText(owner) === "chunk.bytes"
    )
      limits.push(node.right.getText(owner));
    ts.forEachChild(node, findLimits);
  };
  findLimits(owner);
  assert.deepEqual(limits, ["64 * 1024 * 1024"]);

  const parse = (path: string) =>
    ts.createSourceFile(path, read(path), ts.ScriptTarget.Latest, true);
  for (const [path, names] of [
    [
      "workspaces/client/apps/desktop/export-profile.ts",
      ["BrowserProfileArchive", "BrowserProfileSnapshot"],
    ],
    [
      "workspaces/client/apps/desktop/export-browser-profile.ts",
      [
        "BrowserProfileSnapshot",
        "BrowserProfileDatabase",
        "BrowserProfileStore",
        "BrowserProfileChunk",
      ],
    ],
  ] as const) {
    const source = parse(path);
    const declaration = source.statements.find(
      (node) =>
        ts.isImportDeclaration(node) &&
        ts.isStringLiteral(node.moduleSpecifier) &&
        node.moduleSpecifier.text ===
          "../web/src/native/importBrowserProfile.ts",
    );
    assert.ok(declaration && ts.isImportDeclaration(declaration));
    const bindings = declaration.importClause?.namedBindings;
    assert.ok(bindings && ts.isNamedImports(bindings));
    for (const name of names)
      assert.ok(
        bindings.elements.some((element) => element.name.text === name),
        `${path}: ${name}`,
      );
  }
  for (const [path, calls] of [
    [
      "workspaces/client/apps/web/src/native/preferences.ts",
      ["profile.workspace", "profile.chunk"],
    ],
    [
      "workspaces/client/apps/web/src/native/frame.ts",
      ["profile.snapshot", "profile.chunk"],
    ],
  ] as const) {
    const source = parse(path);
    const methods = new Set<string>();
    const visit = (node: ts.Node) => {
      if (ts.isStringLiteral(node)) methods.add(node.text);
      ts.forEachChild(node, visit);
    };
    visit(source);
    for (const method of calls)
      assert.ok(methods.has(method), `${path}: ${method}`);
  }
});
