import { afterEach, describe, expect, it, vi } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { randomUUID } from "./uuid";

afterEach(() => vi.unstubAllGlobals());

function uuidAccessCandidate(source: string): boolean {
  // Escaped identifier and string spellings always contain a backslash.
  if (!/randomUUID|\\/.test(source)) return false;
  const scanner = ts.createScanner(
    ts.ScriptTarget.Latest,
    false,
    ts.LanguageVariant.JSX,
    source,
  );
  while (scanner.scan() !== ts.SyntaxKind.EndOfFileToken) {
    if (scanner.getTokenValue() === "randomUUID") return true;
    // A standalone scanner has no parser context for regex, JSX, or templates.
    // Keep raw names and escapes as candidates even inside an ambiguous token.
    if (/randomUUID|\\/.test(scanner.getTokenText())) return true;
  }
  return false;
}

function directUUIDAccess(source: string): boolean {
  if (!uuidAccessCandidate(source)) return false;
  const tree = ts.createSourceFile(
    "source.tsx",
    source,
    ts.ScriptTarget.Latest,
  );
  let found = false;
  function visit(node: ts.Node) {
    if (
      (ts.isPropertyAccessExpression(node) &&
        node.name.text === "randomUUID") ||
      (ts.isElementAccessExpression(node) &&
        ts.isStringLiteral(node.argumentExpression) &&
        node.argumentExpression.text === "randomUUID") ||
      (ts.isBindingElement(node) &&
        (node.propertyName?.getText(tree) ?? node.name.getText(tree)) ===
          "randomUUID")
    ) {
      found = true;
    }
    ts.forEachChild(node, visit);
  }
  visit(tree);
  return found;
}

describe("browser UUID owner", () => {
  it.each([
    "crypto.randomUUID()",
    "globalThis.crypto?.randomUUID?.()",
    'crypto["randomUUID"]()',
    "const { randomUUID: uuid } = crypto; uuid()",
    String.raw`crypto.r\u0061ndomUUID()`,
    String.raw`crypto["r\u0061ndomUUID"]()`,
    String.raw`crypto["\x72andomUUID"]()`,
    String.raw`const pattern = /"/; crypto.r\u0061ndomUUID()`,
  ])("detects direct browser UUID access: %s", (source) => {
    expect(directUUIDAccess(source)).toBe(true);
  });

  it("does not parse sources without a possible UUID access token", () => {
    expect(
      uuidAccessCandidate("const id = crypto.getRandomValues(bytes);"),
    ).toBe(false);
    expect(directUUIDAccess('const label = "randomUUID";')).toBe(false);
  });

  it("routes every production browser UUID access through src/uuid.ts", () => {
    const root = dirname(fileURLToPath(import.meta.url));
    const violations: string[] = [];
    function visit(directory: string) {
      for (const entry of readdirSync(directory, { withFileTypes: true })) {
        const file = join(directory, entry.name);
        if (entry.isDirectory()) {
          visit(file);
        } else if (
          /\.(?:[cm]?js|tsx?)$/.test(entry.name) &&
          !/\.(?:test|stories)\./.test(entry.name) &&
          file !== join(root, "uuid.ts") &&
          directUUIDAccess(readFileSync(file, "utf8"))
        ) {
          violations.push(file.slice(root.length + 1));
        }
      }
    }
    visit(root);
    expect(violations, "Use src/uuid.ts for browser UUID creation").toEqual([]);
  });

  it("uses the native method with its crypto receiver", () => {
    const crypto = {
      randomUUID: vi.fn(function (this: unknown) {
        expect(this).toBe(crypto);
        return "native-id";
      }),
      getRandomValues: vi.fn(),
    };
    vi.stubGlobal("crypto", crypto);
    expect(randomUUID()).toBe("native-id");
    expect(crypto.getRandomValues).not.toHaveBeenCalled();
  });

  it("creates a UUIDv4 from secure bytes when HTTP lacks randomUUID", () => {
    const crypto = {
      getRandomValues: vi.fn(function (this: unknown, bytes: Uint8Array) {
        expect(this).toBe(crypto);
        bytes.set(Array.from({ length: 16 }, (_, index) => index));
        return bytes;
      }),
    };
    vi.stubGlobal("crypto", crypto);
    expect(randomUUID()).toBe("00010203-0405-4607-8809-0a0b0c0d0e0f");
    expect(crypto.getRandomValues).toHaveBeenCalledTimes(1);
  });

  it("clears the version and variant bits before setting UUIDv4", () => {
    vi.stubGlobal("crypto", {
      getRandomValues: (bytes: Uint8Array) => bytes.fill(0xff),
    });
    expect(randomUUID()).toBe("ffffffff-ffff-4fff-bfff-ffffffffffff");
  });

  it("preserves a secure random source failure", () => {
    const failure = new Error("Secure random source failed");
    vi.stubGlobal("crypto", {
      getRandomValues: () => {
        throw failure;
      },
    });
    expect(() => randomUUID()).toThrow(failure);
  });

  it.each([undefined, {}])(
    "refuses UUID creation without secure randomness (%s)",
    (crypto) => {
      vi.stubGlobal("crypto", crypto);
      const weakRandom = vi.spyOn(Math, "random");
      try {
        expect(() => randomUUID()).toThrow(
          "Secure random values are unavailable",
        );
        expect(weakRandom).not.toHaveBeenCalled();
      } finally {
        weakRandom.mockRestore();
      }
    },
  );
});
