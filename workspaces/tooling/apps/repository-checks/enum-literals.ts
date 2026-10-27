import ts from "typescript";

type Owner = { name: string; members: Map<string, string>; file: string };

// Generated const objects own wire values; do not maintain a second value list here.
function owners(program: ts.Program, files: readonly string[]): Owner[] {
  const result: Owner[] = [];
  for (const file of files) {
    const source = program.getSourceFile(file);
    if (!source) throw new Error(`Missing generated enum owner: ${file}`);
    for (const statement of source.statements) {
      if (
        !ts.isVariableStatement(statement) ||
        !statement.modifiers?.some(
          (m) => m.kind === ts.SyntaxKind.ExportKeyword,
        )
      )
        continue;
      for (const declaration of statement.declarationList.declarations) {
        if (
          !ts.isIdentifier(declaration.name) ||
          !declaration.initializer ||
          !ts.isAsExpression(declaration.initializer) ||
          !ts.isObjectLiteralExpression(declaration.initializer.expression)
        )
          continue;
        const members = new Map<string, string>();
        for (const property of declaration.initializer.expression.properties) {
          if (
            !ts.isPropertyAssignment(property) ||
            !ts.isStringLiteral(property.initializer) ||
            !(
              ts.isIdentifier(property.name) ||
              ts.isStringLiteral(property.name)
            )
          ) {
            members.clear();
            break;
          }
          members.set(property.initializer.text, property.name.text);
        }
        if (members.size)
          result.push({ name: declaration.name.text, members, file });
      }
    }
  }
  return result;
}

function literalValues(type: ts.Type): string[] | undefined {
  const types = type.isUnion() ? type.types : [type];
  const result: string[] = [];
  for (const part of types) {
    if (part.flags & (ts.TypeFlags.Null | ts.TypeFlags.Undefined)) continue;
    if (!part.isStringLiteral()) return undefined;
    result.push(part.value);
  }
  return result;
}

export function enumLiteralFindings(
  program: ts.Program,
  targets: readonly string[],
  ownerFiles: readonly string[],
): string[] {
  const checker = program.getTypeChecker();
  const allOwners = owners(program, ownerFiles);
  const findings: string[] = [];
  function referencesOwner(
    symbol: ts.Symbol | undefined,
    owner: Owner,
    seen = new Set<ts.Symbol>(),
  ): boolean {
    if (!symbol || seen.has(symbol)) return false;
    seen.add(symbol);
    if (symbol.flags & ts.SymbolFlags.Alias)
      symbol = checker.getAliasedSymbol(symbol);
    for (const declaration of symbol.declarations ?? []) {
      if (declaration.getSourceFile().fileName === owner.file) return true;
      let found = false;
      const walk = (node: ts.Node) => {
        if (found) return;
        if (
          ts.isIdentifier(node) &&
          referencesOwner(checker.getSymbolAtLocation(node), owner, seen)
        ) {
          found = true;
          return;
        }
        ts.forEachChild(node, walk);
      };
      // Look only at type annotations, never at implementation text.
      if (
        "type" in declaration &&
        declaration.type &&
        ts.isTypeNode(declaration.type as ts.Node)
      )
        walk(declaration.type as ts.Node);
      if (found) return true;
    }
    return false;
  }
  for (const file of targets) {
    const source = program.getSourceFile(file);
    if (!source || ownerFiles.includes(file)) continue;
    const visit = (node: ts.Node) => {
      if (ts.isStringLiteral(node) && !ts.isLiteralTypeNode(node.parent)) {
        const parent = node.parent;
        let type: ts.Type | undefined;
        let symbol: ts.Symbol | undefined;
        if (
          ts.isBinaryExpression(parent) &&
          [
            ts.SyntaxKind.EqualsEqualsToken,
            ts.SyntaxKind.EqualsEqualsEqualsToken,
            ts.SyntaxKind.ExclamationEqualsToken,
            ts.SyntaxKind.ExclamationEqualsEqualsToken,
          ].includes(parent.operatorToken.kind)
        ) {
          const other = parent.left === node ? parent.right : parent.left;
          type = checker.getTypeAtLocation(other);
          symbol = checker.getSymbolAtLocation(other);
        } else if (
          ts.isCaseClause(parent) &&
          ts.isCaseBlock(parent.parent) &&
          ts.isSwitchStatement(parent.parent.parent)
        ) {
          const expression = parent.parent.parent.expression;
          type = checker.getTypeAtLocation(expression);
          symbol = checker.getSymbolAtLocation(expression);
        } else {
          type = checker.getContextualType(node);
          if (
            ts.isPropertyAssignment(parent) &&
            ts.isObjectLiteralExpression(parent.parent)
          ) {
            const context = checker.getContextualType(parent.parent);
            if (
              context &&
              (ts.isIdentifier(parent.name) || ts.isStringLiteral(parent.name))
            )
              symbol = checker.getPropertyOfType(context, parent.name.text);
          } else if (ts.isCallExpression(parent)) {
            const index = parent.arguments.indexOf(node);
            symbol = checker.getResolvedSignature(parent)?.parameters[index];
          } else if (ts.isVariableDeclaration(parent))
            symbol = checker.getSymbolAtLocation(parent.name);
        }
        const values = type && literalValues(type);
        const matches =
          values &&
          allOwners.filter(
            (owner) =>
              values.length === owner.members.size &&
              values.every((value) => owner.members.has(value)) &&
              owner.members.has(node.text) &&
              (referencesOwner(type?.aliasSymbol, owner) ||
                referencesOwner(symbol, owner)),
          );
        if (matches?.length) {
          const pos = source.getLineAndCharacterOfPosition(node.getStart());
          const alternatives = matches
            .map(
              (owner) =>
                `${owner.name}[${JSON.stringify(owner.members.get(node.text))}] from ${owner.file}`,
            )
            .join(" or ");
          findings.push(
            `${file}:${pos.line + 1}:${pos.character + 1}: import and use ${alternatives} instead of ${JSON.stringify(node.text)}`,
          );
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  return findings;
}
