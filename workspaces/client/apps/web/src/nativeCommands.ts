import {
  ProviderKind,
  isProviderKind,
  providerSupports,
} from "./generated/providerKind.ts";
import type { NativeAction } from "./generated/api.ts";
export type { NativeAction } from "./generated/api.ts";
// Studio handles these chat commands itself. A Claude chat sends /compact and
// /review to Claude as its own commands; native review runs only on Codex.

const STUDIO_COMMANDS: Record<ProviderKind, RegExp> = {
  [ProviderKind.Codex]: /^\/(rename|compact|review|stop|stop-team)(\s|$)/,
  [ProviderKind.Claude]: /^\/(rename|stop|stop-team)(\s|$)/,
};
// Native review runs only on Codex. Compact follows the compaction capability.
const REVIEW_PROVIDERS: ReadonlySet<ProviderKind> = new Set([
  ProviderKind.Codex,
]);

export function renameCommand(text: string): string | null | undefined {
  const match = /^\/rename(?:\s+(.*))?$/s.exec(text);
  return match ? match[1]?.trim() || null : undefined;
}

export function studioCommand(
  text: string,
  provider: string | undefined,
): boolean {
  return isProviderKind(provider) && STUDIO_COMMANDS[provider].test(text);
}

export function menuActions(provider: string | undefined): NativeAction[] {
  if (!isProviderKind(provider)) return [];
  const actions: NativeAction[] = [];
  if (providerSupports(provider, "compaction")) actions.push("compact");
  if (REVIEW_PROVIDERS.has(provider)) actions.push("review");
  return actions;
}
