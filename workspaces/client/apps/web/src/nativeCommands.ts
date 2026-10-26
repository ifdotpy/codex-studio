import { ProviderKind, isProviderKind } from "./generated/providerKind.ts";
// Studio handles these chat commands itself. A Claude chat sends /compact and
// /review to Claude as its own commands; native review runs only on Codex.
export type NativeAction = "compact" | "review";

const STUDIO_COMMANDS: Record<ProviderKind, RegExp> = {
  [ProviderKind.Codex]: /^\/(rename|compact|review|stop|stop-team)(\s|$)/,
  [ProviderKind.Claude]: /^\/(rename|stop|stop-team)(\s|$)/,
};
const MENU_ACTIONS: Record<ProviderKind, NativeAction[]> = {
  [ProviderKind.Codex]: ["compact", "review"],
  [ProviderKind.Claude]: ["compact"],
};

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
  return isProviderKind(provider) ? [...MENU_ACTIONS[provider]] : [];
}
