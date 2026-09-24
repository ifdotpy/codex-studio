// Studio handles these chat commands itself. A Claude chat sends /compact and
// /review to Claude as its own commands; native review runs only on Codex.
export type NativeAction = "compact" | "review";

export function studioCommand(
  text: string,
  provider: string | undefined,
): boolean {
  const commands =
    provider === "claude"
      ? /^\/(stop|stop-team)(\s|$)/
      : /^\/(compact|review|stop|stop-team)(\s|$)/;
  return commands.test(text);
}

export function menuActions(provider: string | undefined): NativeAction[] {
  return provider === "claude" ? ["compact"] : ["compact", "review"];
}
