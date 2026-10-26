// @ts-check
// Stable VM and host entrypoint; this check precedes every TypeScript import.
if (!process.features.typescript) {
  console.error(
    "Claude bridge requires Node.js with built-in TypeScript support enabled (Node.js 22.18 or later). See workspaces/providers/apps/claude-bridge/README.md#installation-requirements.",
  );
  process.exit(1);
}
await import(
  process.argv[2] === "--refresh-auth" ? "./refresh-auth.ts" : "./bridge.ts"
);
