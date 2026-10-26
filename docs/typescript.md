# TypeScript

Code that runs on Node, Electron, Vite, Vitest, or Playwright is TypeScript:
product code, tests, fixtures, scripts, and tool configuration.

## Where types come from

- Rust owns every contract that crosses the server boundary. The
  [runtime schema](../workspaces/runtime/packages/runtime-schema/README.md)
  generates its TypeScript. Import the generated type or its named alias; never
  restate a generated shape. When a Rust-owned shape has no generated type, add
  it to the generator instead of writing a TypeScript copy.
- A TypeScript-only type, constant, or helper has one owning module. Other
  modules, tests, and fixtures import it.
- Test data names its type from the owner, for example with `satisfies`, so a
  contract change fails the type check instead of leaving a stale fixture.
- Code in this repository has no hand-written declaration file (`.d.ts`,
  `.d.mts`). Write the module in TypeScript; declarations describe external code
  only.
- Code that a test serves to a browser lives in a real `.ts` or `.tsx` module,
  not in a string, so the type check covers it against the live code.

## Not allowed

- `any`, `@ts-ignore`, and `@ts-nocheck`. Use `unknown` and narrow it.
  `@ts-expect-error` is allowed only in a test, on a value that deliberately
  violates its declared type, with the reason.
- A cast (`as`) or non-null assertion (`!`) that hides a mismatch. Narrow with a
  check instead; cast only after a runtime check of untrusted input.
- A copied type, constant, or helper instead of an import of its owner.

## How TypeScript runs

Node and Electron run `.ts` files directly by removing the types, so there is
no build step. Vite, Vitest, and Playwright compile TypeScript themselves.
Every Node that runs Studio code, including the Node on a server host, in a VM
guest, or used by the native app's recovery helper, must therefore be Node.js
22.18 or later.
Code therefore uses only syntax that can be removed without a transform: no
`enum`, `namespace`, or constructor parameter properties. Type-only imports use
`import type`, and relative imports name the `.ts` file.
[`tsconfig.base.json`](../tsconfig.base.json) owns these compiler settings.
Every project extends it, and `just test-fast` checks every project.

## JavaScript exceptions

A file stays JavaScript only when its runtime loads that file unchanged and has
no TypeScript loader, for example a sandboxed preload script or a file embedded
by the Rust build. Such a file starts with `// @ts-check`, takes its types from
their TypeScript owners, and belongs to a checked project.
