import ts from "typescript";

export type GeneratedTypeDiagnostic = {
  file: string;
  text: string;
  message: string;
};

export function switchDefaultDiagnostics(
  program: ts.Program,
  targets: readonly string[],
) {
  const checker = program.getTypeChecker();
  const findings: { file: string; text: string; message: string }[] = [];
  for (const file of targets) {
    const source = program.getSourceFile(file);
    if (!source) throw new Error(`Missing switch target: ${file}`);
    const visit = (node: ts.Node) => {
      if (
        ts.isSwitchStatement(node) &&
        ts.isPropertyAccessExpression(node.expression)
      ) {
        const type = checker.getTypeAtLocation(node.expression);
        const clauses = node.caseBlock.clauses;
        const fallback = clauses.find(ts.isDefaultClause);
        if (
          fallback &&
          type.isUnion() &&
          type.types.length > 1 &&
          type.types.every(
            (part) => part.isStringLiteral() || part.isNumberLiteral(),
          )
        ) {
          let last = fallback.statements.at(-1);
          while (last && ts.isBlock(last)) last = last.statements.at(-1);
          let safe =
            !!last && (ts.isReturnStatement(last) || ts.isThrowStatement(last));
          if (last && ts.isVariableStatement(last)) {
            safe ||= last.declarationList.declarations.some(
              (declaration) =>
                !!declaration.type &&
                !!declaration.initializer &&
                declaration.initializer.getText(source) ===
                  node.expression.getText(source) &&
                !!(
                  checker.getTypeFromTypeNode(declaration.type).flags &
                  ts.TypeFlags.Never
                ) &&
                !!(
                  checker.getTypeAtLocation(declaration.initializer).flags &
                  ts.TypeFlags.Never
                ),
            );
          }
          if (
            last &&
            ts.isExpressionStatement(last) &&
            ts.isCallExpression(last.expression)
          ) {
            const call = last.expression;
            const signature = checker.getResolvedSignature(call);
            safe ||= !!signature?.parameters.some((parameter, index) => {
              const argument = call.arguments[index];
              const declaration = parameter.valueDeclaration;
              return (
                !!argument &&
                argument.getText(source) === node.expression.getText(source) &&
                !!declaration &&
                !!(
                  checker.getTypeOfSymbolAtLocation(parameter, declaration)
                    .flags & ts.TypeFlags.Never
                ) &&
                !!(
                  checker.getTypeAtLocation(argument).flags & ts.TypeFlags.Never
                )
              );
            });
          }
          if (!safe)
            findings.push({
              file,
              text: node.getText(source),
              message:
                "Return, throw, or assert the remaining value is never in the default branch",
            });
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  return findings;
}

// Compare declarations against the generated source, including OpenAPI schema
// properties, without treating a type derived from the owner as a copy.
export function generatedTypeDiagnostics(
  program: ts.Program,
  targets: readonly string[],
  ownerFiles: readonly string[],
): GeneratedTypeDiagnostic[] {
  const checker = program.getTypeChecker();
  const names = new Map<string, string>();
  const unions = new Map<string, string>();
  function unionKey(node: ts.Node): string | undefined {
    if (!ts.isUnionTypeNode(node) || node.types.length < 2) return;
    const values: string[] = [];
    for (const part of node.types) {
      if (!ts.isLiteralTypeNode(part) || !ts.isStringLiteral(part.literal))
        return;
      values.push(part.literal.text);
    }
    return JSON.stringify([...new Set(values)].sort());
  }
  for (const file of ownerFiles) {
    const source = program.getSourceFile(file);
    if (!source) throw new Error(`Missing generated type owner: ${file}`);
    for (const statement of source.statements) {
      if (
        (ts.isTypeAliasDeclaration(statement) ||
          ts.isInterfaceDeclaration(statement) ||
          ts.isEnumDeclaration(statement)) &&
        statement.modifiers?.some(
          (modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword,
        )
      ) {
        names.set(statement.name.text, file);
      }
    }
    const visit = (node: ts.Node) => {
      if (ts.isTypeAliasDeclaration(node)) {
        const values = literalValues(checker.getTypeFromTypeNode(node.type));
        if (values && values.length > 1)
          unions.set(
            JSON.stringify([...new Set(values)].sort()),
            `${node.name.text} in ${file}`,
          );
      }
      if (
        ts.isPropertySignature(node) &&
        ts.isTypeLiteralNode(node.parent) &&
        ts.isPropertySignature(node.parent.parent) &&
        node.parent.parent.name.getText(source) === "schemas" &&
        node.name
      ) {
        const name =
          ts.isIdentifier(node.name) || ts.isStringLiteral(node.name)
            ? node.name.text
            : undefined;
        if (name) names.set(name, file);
      }
      const key = unionKey(node);
      if (key) {
        const parent = node.parent;
        const name =
          (ts.isTypeAliasDeclaration(parent) ||
            ts.isPropertySignature(parent)) &&
          parent.name
            ? parent.name.getText(source)
            : "union";
        unions.set(key, `${name} in ${file}`);
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  const findings: GeneratedTypeDiagnostic[] = [];
  for (const file of targets) {
    if (ownerFiles.includes(file)) continue;
    const source = program.getSourceFile(file);
    if (!source) throw new Error(`Missing generated type target: ${file}`);
    const visit = (node: ts.Node) => {
      if (
        ts.isTypeAliasDeclaration(node) ||
        ts.isInterfaceDeclaration(node) ||
        ts.isEnumDeclaration(node)
      ) {
        const name = node.name.text;
        const owner = names.get(name);
        const union = ts.isTypeAliasDeclaration(node)
          ? unions.get(unionKey(node.type) ?? "")
          : undefined;
        if (owner || union)
          findings.push({
            file,
            text: node.getText(source),
            message: owner
              ? `Import ${name} from ${owner}`
              : `Derive ${name} from ${union}`,
          });
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  return findings;
}

type Owner = {
  name: string;
  component: string;
  members: Map<string, string>;
  file: string;
};

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
              ts.isStringLiteral(property.name) ||
              (ts.isComputedPropertyName(property.name) &&
                ts.isStringLiteral(property.name.expression))
            )
          ) {
            members.clear();
            break;
          }
          const name = ts.isComputedPropertyName(property.name)
            ? (property.name.expression as ts.StringLiteral).text
            : property.name.text;
          members.set(property.initializer.text, name);
        }
        if (members.size)
          result.push({
            name: declaration.name.text,
            component: componentName(source, declaration.name.text),
            members,
            file,
          });
      }
    }
  }
  return result;
}

function componentName(source: ts.SourceFile, name: string): string {
  const alias = source.statements.find(
    (statement) =>
      ts.isTypeAliasDeclaration(statement) && statement.name.text === name,
  );
  if (
    alias &&
    ts.isTypeAliasDeclaration(alias) &&
    ts.isIndexedAccessTypeNode(alias.type) &&
    ts.isLiteralTypeNode(alias.type.indexType) &&
    ts.isStringLiteral(alias.type.indexType.literal)
  )
    return alias.type.indexType.literal.text;
  return name;
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

export type EnumLiteralDiagnostic = {
  file: string;
  line: number;
  column: number;
  owner: string;
  ownerFile: string;
  member: string;
  literal: string;
};

export function enumLiteralDiagnostics(
  program: ts.Program,
  targets: readonly string[],
  ownerFiles: readonly string[],
): EnumLiteralDiagnostic[] {
  const checker = program.getTypeChecker();
  const allOwners = owners(program, ownerFiles);
  const findings: EnumLiteralDiagnostic[] = [];
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
      if (
        declaration.getSourceFile().fileName === owner.file &&
        "name" in declaration &&
        declaration.name &&
        (ts.isIdentifier(declaration.name as ts.Node) ||
          ts.isStringLiteral(declaration.name as ts.Node))
      ) {
        const name = (declaration.name as ts.Identifier | ts.StringLiteral)
          .text;
        const schemaProperty =
          ts.isPropertySignature(declaration) &&
          ts.isTypeLiteralNode(declaration.parent) &&
          ts.isPropertySignature(declaration.parent.parent) &&
          declaration.parent.parent.name.getText() === "schemas";
        if (
          (schemaProperty && name === owner.component) ||
          ((ts.isTypeAliasDeclaration(declaration) ||
            ts.isVariableDeclaration(declaration)) &&
            name === owner.name)
        )
          return true;
      }
      let found = false;
      const walk = (node: ts.Node) => {
        if (found) return;
        if (
          ts.isIndexedAccessTypeNode(node) &&
          ts.isLiteralTypeNode(node.indexType) &&
          ts.isStringLiteral(node.indexType.literal)
        ) {
          const member = checker.getPropertyOfType(
            checker.getTypeAtLocation(node.objectType),
            node.indexType.literal.text,
          );
          if (referencesOwner(member, owner, seen)) found = true;
          return;
        }
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
        const declaration =
          symbol?.valueDeclaration ?? symbol?.declarations?.[0];
        const declaredType =
          symbol && declaration
            ? checker.getTypeOfSymbolAtLocation(symbol, declaration)
            : undefined;
        const values =
          (declaredType && literalValues(declaredType)) ??
          (type && literalValues(type));
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
          for (const owner of matches) {
            findings.push({
              file,
              line: pos.line + 1,
              column: pos.character + 1,
              owner: owner.name,
              ownerFile: owner.file,
              member: owner.members.get(node.text)!,
              literal: node.text,
            });
          }
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  return findings;
}

export function enumLiteralFindings(
  program: ts.Program,
  targets: readonly string[],
  ownerFiles: readonly string[],
): string[] {
  return enumLiteralDiagnostics(program, targets, ownerFiles).map(
    (finding) =>
      `${finding.file}:${finding.line}:${finding.column}: import and use ${finding.owner}[${JSON.stringify(finding.member)}] from ${finding.ownerFile} instead of ${JSON.stringify(finding.literal)}`,
  );
}
